//! pmgfand デーモン本体。
//! Phase 2: 監視 + Unix socket。Phase 3/4: 制御ループ（Fixed PWM / iRMC Auto / Curve）。

use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use pmgfan_core::control::{ControlParams, PiController, PiParams, PwmScope, RateLimiter};
use pmgfan_core::curve::Curve;
use pmgfan_core::fan::FanReading;
use pmgfan_core::hwmon;
use pmgfan_core::protocol::{CalibPoint, CalibStatus, CurveSpec, DaemonState, Mode};
use pmgfan_core::sensor::{self, TempReading};
use pmgfan_ipmi::backend::FanControlBackend;
use tokio::net::UnixDatagram;
use tokio::sync::{watch, Mutex, RwLock};
use tokio::time::{interval, MissedTickBehavior};
use tracing::{error, info, warn};

use crate::ipc;

/// PWM 値が変わらなくても強制値を再送する間隔（適用 tick 数）。
const REASSERT_TICKS: u32 = 6;
/// 緊急温度超過時に強制する PWM。
const EMERGENCY_PWM: u8 = 100;
/// シャットダウン時に制御ループの終了を待つ上限。
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(5);

/// フェイルアクション（監視系の致命的失敗時の挙動）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailAction {
    /// OEM override を解除して iRMC 自動制御へ戻す（既定）
    IrmcAuto,
    /// 100% PWM を強制する（iRMC 自体が信用できない場合向け）
    FullSpeed,
}

impl FailAction {
    pub fn parse(s: &str) -> std::result::Result<Self, String> {
        match s {
            "irmc-auto" | "irmc_auto" => Ok(Self::IrmcAuto),
            "full-speed" | "full_speed" => Ok(Self::FullSpeed),
            other => Err(format!(
                "unknown fail_action '{other}' (expected \"irmc-auto\" or \"full-speed\")"
            )),
        }
    }
}

/// デーモン動作パラメータ（config または既定値から構築）。
#[derive(Debug, Clone)]
pub struct Params {
    pub socket_path: PathBuf,
    pub fan_interval: Duration,
    pub temp_interval: Duration,
    pub apply_interval: Duration,
    pub control: ControlParams,
    /// PI 制御パラメータ（[target_rpm] + [control] min/max から構築）
    pub pi: PiParams,
    /// 起動時に読み込んだキャリブレーション表（PI の初期 PWM 推定用）
    pub calibration: Vec<CalibPoint>,
    /// キャリブレーション結果の保存先
    pub calibration_path: PathBuf,
    pub curves: Vec<Curve>,
    pub startup_mode: Mode,
    pub ipmi_failure_limit: u32,
    pub cpu_emergency: f32,
    pub pch_emergency: f32,
    /// 温度データがこの期間更新されなければフェイルとみなす
    pub sensor_stale: Duration,
    /// センサー陳腐化・0 RPM 時のアクション
    pub fail_action: FailAction,
    /// 起動時に FRU 製品名を照合する期待値
    pub expected_model: String,
    /// 起動に使った設定ファイル。SetCurves の永続化先
    /// （None なら実行時適用のみでファイルは書かない）
    pub config_path: Option<PathBuf>,
}

/// デーモンの共有状態。
#[derive(Debug)]
pub struct Shared {
    pub state: DaemonState,
    /// 現在の要求モード（IPC set_mode で変更される）
    pub mode: Mode,
    /// mode 変更ごとにインクリメント。制御ループが
    /// 変化を検知してリミッタをリセットする
    pub mode_generation: u64,
    /// 現在書き込んでいる PWM。強制系でなければ None
    pub pwm: Option<u8>,
    pub fans: Vec<FanReading>,
    pub temps: Vec<TempReading>,
    /// 直近の IPMI エラー（診断用）
    pub last_error: Option<String>,
    /// 読み取り系の連続失敗。それぞれ独立して数え、
    /// 片方の成功で他方をリセットしない
    pub fan_failures: u32,
    pub temp_failures: u32,
    /// 書き込み系の連続失敗
    pub write_failures: u32,
    /// `clear_override` が未達。`pwm=None` だが実際の強制状態が
    /// 不明なことを示し、mode が IrmcAuto の間制御ループが
    /// 解除を再試行する
    pub clear_pending: bool,
    /// Curve モードで参照センサーが1つも解決できない。
    /// `refresh_state` が Degraded 判定に使う（制御ループが
    /// 毎 tick 真偽を更新し、回復もこのフラグ経由で行う。
    /// mode が Curve 以外の時は無視される）
    pub curve_sensors_missing: bool,
    /// 最後に「非空の」温度データがコミットされた時刻。
    /// これが `sensor_stale` を超えるとフェイルアクション発動
    pub last_temp_ok: Instant,
    /// 温度データ陳腐化フラグ（制御ループが毎 tick 更新）
    pub sensor_stale: bool,
    /// 有効なファンが 0 RPM を連続して報告している（ポーリングが管理）
    pub zero_rpm_detected: bool,
    /// 0 RPM を観測した連続ポーリング回数
    pub zero_rpm_count: u32,
    /// 最後にファン読み取りが成功した時刻（watchdog の鮮度判定用）
    pub last_poll_ok: Instant,
    /// 現在有効なファンカーブ（SetCurves で実行時更新される）
    pub curves: Vec<Curve>,
    /// 現在有効な PWM スコープ（SetPwmScope で実行時更新される。
    /// ControlParams 側は起動時の値で固定のため、制御ループは
    /// 常にこちらを参照する）
    pub pwm_scope: PwmScope,
    /// TargetRpm モードで参照ファンが解決不能（制御ループが毎 tick
    /// 更新。Curve 以外のモード時は無視される）
    pub target_fan_missing: bool,
    /// キャリブレーション進行状態（StartCalibration で開始、
    /// モード変更で中断。status レスポンスに含める）
    pub calibration: Option<CalibStatus>,
    /// キャリブレーション完了後に戻るモード
    pub calib_resume_mode: Mode,
    pub started: Instant,
}

impl Shared {
    fn new(startup_mode: Mode, curves: Vec<Curve>, pwm_scope: PwmScope) -> Self {
        Self {
            state: DaemonState::Starting,
            mode: startup_mode,
            curves,
            pwm_scope,
            target_fan_missing: false,
            calibration: None,
            calib_resume_mode: Mode::IrmcAuto,
            mode_generation: 0,
            pwm: None,
            fans: Vec::new(),
            temps: Vec::new(),
            last_error: None,
            fan_failures: 0,
            temp_failures: 0,
            write_failures: 0,
            clear_pending: false,
            curve_sensors_missing: false,
            last_temp_ok: Instant::now(),
            sensor_stale: false,
            zero_rpm_detected: false,
            zero_rpm_count: 0,
            last_poll_ok: Instant::now(),
            started: Instant::now(),
        }
    }
}

/// mode に対応する「正常時」の状態。
fn normal_state(mode: &Mode) -> DaemonState {
    match mode {
        Mode::IrmcAuto => DaemonState::Monitoring,
        _ => DaemonState::Controlling,
    }
}

/// 失敗カウンタ・clear_pending から `state` を再計算する。
/// どれか一つでも閾値超過・clear 未達なら `Degraded`。
/// `Failsafe`（緊急温度）は制御ループが管理するため、
/// ここでは触らない（現状維持）。
fn refresh_state(s: &mut Shared, failure_limit: u32) {
    if s.state == DaemonState::Failsafe {
        return;
    }
    let degraded = s.fan_failures >= failure_limit
        || s.temp_failures >= failure_limit
        || s.write_failures >= failure_limit
        || s.clear_pending
        || s.sensor_stale
        || s.zero_rpm_detected
        || (matches!(s.mode, Mode::Curve) && s.curve_sensors_missing)
        || (matches!(s.mode, Mode::TargetRpm { .. }) && s.target_fan_missing);
    s.state = if degraded {
        DaemonState::Degraded
    } else {
        normal_state(&s.mode)
    };
}

/// `last_error` を消してよい健全状態か（診断が意味を持つ
/// `Degraded`/`Failsafe` の間は消さない）。
fn is_healthy_state(state: DaemonState) -> bool {
    !matches!(state, DaemonState::Degraded | DaemonState::Failsafe)
}

/// デーモンを起動し、シャットダウンまでブロックする。
///
/// 終了時（SIGTERM/SIGINT 含む）は必ず OEM override を解除して
/// iRMC 自動制御へ戻す（フェイルセーフ。docs/04-safety.md）。
pub async fn run<B>(backend: B, params: Params) -> Result<()>
where
    B: FanControlBackend + Send + Sync + 'static,
{
    let backend = Arc::new(backend);
    let shared = Arc::new(RwLock::new(Shared::new(
        params.startup_mode.clone(),
        params.curves.clone(),
        params.control.pwm_scope,
    )));
    let socket_path = params.socket_path.clone();

    // 単一インスタンスロック。2重起動は IPMI 操作・socket 掃除で
    // 互いに干渉するため、起動直後に fail-fast で防ぐ。
    let run_dir = socket_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/run/pmgfand"));
    let _instance_lock = acquire_instance_lock(&run_dir)?;

    // 機種検証: 想定外の機種に OEM raw コマンドを送らない
    // （docs/02「想定外であれば制御を開始しない」）。
    match backend.model_name().await {
        Ok(model) if model.contains(&params.expected_model) => {
            info!(model = %model, "product model verified");
        }
        Ok(model) => bail!(
            "unexpected product '{model}' (expected '{}')",
            params.expected_model
        ),
        Err(e) => bail!("cannot verify product model: {e}"),
    }

    // 起動時に iRMC の強制状態を一度だけ正規化する。
    // W 書き込みはスコープ外の強制状態を変更しないため、前回の
    // 実行が残した override（例: all スコープで強制された PSU）は
    // chassis スコープへの書き込みでは解除されず固着する。
    // 失敗時は clear_pending を立て、制御ループが再試行する。
    if let Err(e) = backend.clear_override().await {
        warn!(error = %e, "startup clear_override failed; stale force may persist");
        shared.write().await.clear_pending = true;
    }

    // PWM/override 操作の直列化ロック。制御ループの書き込みと
    // apply_mode の即時解除が入れ違いで「iRMC Auto 表示のまま
    // override が残る」ことを防ぐ。
    let ctrl = Arc::new(Mutex::new(()));

    // 終了通知（watch = ラッチ付き。送った後に受信側が
    // notified を待ち始めても見逃さない。abort すると ipmitool
    // 子プロセスが孤児になり clear 後に強制値が着地する可能性
    // があるため、制御ループは graceful に終わらせる）。
    let (sd_tx, sd_rx) = watch::channel(false);

    // ファン RPM ポーリング
    let poll_fans = {
        let backend = Arc::clone(&backend);
        let shared = Arc::clone(&shared);
        let shutdown = sd_rx.clone();
        let fan_interval = params.fan_interval;
        let limit = params.ipmi_failure_limit;
        tokio::spawn(async move {
            poll_fans_loop(&*backend, &shared, shutdown, fan_interval, limit).await
        })
    };

    // 温度ポーリング（IPMI + hwmon）
    let poll_temps = {
        let backend = Arc::clone(&backend);
        let shared = Arc::clone(&shared);
        let shutdown = sd_rx.clone();
        let temp_interval = params.temp_interval;
        let limit = params.ipmi_failure_limit;
        tokio::spawn(async move {
            poll_temps_loop(&*backend, &shared, shutdown, temp_interval, limit).await
        })
    };

    // 制御ループ（PWM 書き込みの単一主体）
    let control = {
        let backend = Arc::clone(&backend);
        let shared = Arc::clone(&shared);
        let ctrl = Arc::clone(&ctrl);
        let apply_interval = params.apply_interval;
        let control_params = params.control;
        let pi_params = params.pi;
        let calib_table = params.calibration.clone();
        let calib_path = params.calibration_path.clone();
        let limit = params.ipmi_failure_limit;
        let (cpu_em, pch_em) = (params.cpu_emergency, params.pch_emergency);
        let stale_secs = params.sensor_stale.as_secs();
        let fail_action = params.fail_action;
        tokio::spawn(async move {
            control_loop(
                &*backend,
                &shared,
                &ctrl,
                sd_rx,
                control_params,
                pi_params,
                calib_table,
                calib_path,
                apply_interval,
                limit,
                cpu_em,
                pch_em,
                stale_secs,
                fail_action,
            )
            .await
        })
    };

    // Unix socket（READY=1 より先に bind を完了させる）
    let listener = ipc::bind(&socket_path)?;
    let ipc_task = {
        let backend = Arc::clone(&backend);
        let shared = Arc::clone(&shared);
        let ctrl = Arc::clone(&ctrl);
        let control_params = params.control;
        let config_path = params.config_path.clone();
        tokio::spawn(async move {
            ipc::serve(listener, backend, shared, ctrl, control_params, config_path).await
        })
    };

    // systemd watchdog（WATCHDOG_USEC があれば半周期でキック。
    // ただしファン読み取りの鮮度が保たれている間だけ送る）
    spawn_watchdog(Arc::clone(&shared));

    // 起動モードを反映
    let startup_is_auto = {
        let s = shared.read().await;
        s.mode == Mode::IrmcAuto
    };
    shared.write().await.state = if startup_is_auto {
        DaemonState::Monitoring
    } else {
        DaemonState::Controlling
    };
    if startup_is_auto {
        // 前回プロセスの強制が残っている可能性を潰す。
        // 制御ループと直列化して実行する。
        let _ctrl = ctrl.lock().await;
        if let Err(e) = backend.clear_override().await {
            warn!(error = %e, "startup clear_override failed");
            let mut s = shared.write().await;
            // 解除未達を記録し、制御ループの IrmcAuto リトライに委ねる
            s.clear_pending = true;
            s.state = DaemonState::Degraded;
            s.last_error = Some(e.to_string());
        }
    }
    sd_notify("READY=1").await;
    info!(socket = %socket_path.display(), mode = ?params.startup_mode, "pmgfand started");

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut poll_fans = poll_fans;
    let mut poll_temps = poll_temps;
    let mut control = control;
    let mut ipc_task = ipc_task;
    // どこかのタスクが先に終わった場合は異常終了として扱い、
    // systemd に再起動させる（exit code != 0）。
    let fatal: Option<String> = tokio::select! {
        _ = tokio::signal::ctrl_c() => { info!("SIGINT received"); None }
        _ = sigterm.recv() => { info!("SIGTERM received"); None }
        r = &mut poll_fans => Some(format!("fan poller exited: {r:?}")),
        r = &mut poll_temps => Some(format!("temp poller exited: {r:?}")),
        r = &mut control => Some(format!("control loop exited: {r:?}")),
        r = &mut ipc_task => Some(format!("ipc server exited: {r:?}")),
    };
    if let Some(e) = &fatal {
        error!("{e}");
    }

    // シャットダウン: 制御ループに終了を通知し、in-flight の
    // 書き込みが完了するのを待ってから override を解除する。
    info!("shutting down; clearing OEM override");
    let _ = sd_tx.send(true);
    if tokio::time::timeout(SHUTDOWN_JOIN_TIMEOUT, &mut control)
        .await
        .is_err()
    {
        warn!("control loop did not stop in time; aborting");
        control.abort();
    }
    poll_fans.abort();
    poll_temps.abort();
    ipc_task.abort();
    {
        // 制御ループの書き込みと直列化（ここに来る時点で既に
        // 止まっているはずだが、apply_mode の解除と順序を合わせる）
        let _ctrl = ctrl.lock().await;
        if let Err(e) = backend.clear_override().await {
            error!(error = %e, "failed to clear override on shutdown");
        }
    }
    sd_notify("STOPPING=1").await;
    let _ = std::fs::remove_file(&socket_path);

    match fatal {
        Some(e) => bail!(e),
        None => Ok(()),
    }
}

async fn poll_fans_loop<B: FanControlBackend>(
    backend: &B,
    shared: &RwLock<Shared>,
    mut shutdown: watch::Receiver<bool>,
    poll_interval: Duration,
    failure_limit: u32,
) {
    let mut tick = interval(poll_interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            // sender が send なしで drop された場合も busy-loop
            // にならないよう Err を終了扱いにする
            r = shutdown.changed() => {
                if r.is_err() {
                    return;
                }
            }
        }
        if *shutdown.borrow() {
            return;
        }
        // IPMI 呼び出しはロックの外で行い、結果のコミットだけ
        // ロックを取る（ロック越し IO は解除操作をブロックする）。
        let result = backend.fans().await;
        let mut s = shared.write().await;
        match result {
            Ok(fans) => {
                // 有効ファンが 0 RPM を報告し続けるか観測する。
                // 回転数自体を返さないファン（Disabled/nr）は
                // rpm=None なので自然に除外される
                let zero_fans: Vec<&str> = fans
                    .iter()
                    .filter(|f| f.rpm == Some(0))
                    .map(|f| f.name.as_str())
                    .collect();
                if zero_fans.is_empty() {
                    s.zero_rpm_count = 0;
                    s.zero_rpm_detected = false;
                } else {
                    s.zero_rpm_count += 1;
                    if s.zero_rpm_count >= failure_limit && !s.zero_rpm_detected {
                        s.zero_rpm_detected = true;
                        s.last_error =
                            Some(format!("fan(s) reporting 0 RPM: {}", zero_fans.join(", ")));
                        warn!(fans = ?zero_fans, "zero-RPM fan detected");
                    }
                }
                s.fans = fans;
                s.fan_failures = 0;
                s.last_poll_ok = Instant::now();
                refresh_state(&mut s, failure_limit);
                // 他ドメインの診断を消さないよう、健全な場合だけクリア
                if is_healthy_state(s.state) {
                    s.last_error = None;
                }
            }
            Err(e) => {
                s.fan_failures += 1;
                s.last_error = Some(e.to_string());
                if s.fan_failures >= failure_limit {
                    warn!(failures = s.fan_failures, error = %e, "ipmi fan polling failing");
                }
                refresh_state(&mut s, failure_limit);
            }
        }
    }
}

async fn poll_temps_loop<B: FanControlBackend>(
    backend: &B,
    shared: &RwLock<Shared>,
    mut shutdown: watch::Receiver<bool>,
    poll_interval: Duration,
    failure_limit: u32,
) {
    let mut tick = interval(poll_interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            // sender が send なしで drop された場合も busy-loop
            // にならないよう Err を終了扱いにする
            r = shutdown.changed() => {
                if r.is_err() {
                    return;
                }
            }
        }
        if *shutdown.borrow() {
            return;
        }
        let result = backend.temperatures().await;
        // sysfs 同期読み取りはランタイムスレッドを塞がないよう別スレッドへ
        let hw = tokio::task::spawn_blocking(read_hwmon)
            .await
            .unwrap_or_default();
        let ok = result.is_ok();
        let mut s = shared.write().await;
        match result {
            Ok(mut temps) => {
                temps.extend(hw);
                // 「非空の」温度データが届いた時刻のみ鮮度を更新
                // （空レスポンスは陳腐化判定でフェイル扱いにする）
                if !temps.is_empty() {
                    s.last_temp_ok = Instant::now();
                }
                s.temps = temps;
                s.temp_failures = 0;
            }
            Err(e) => {
                // IPMI 温度が取れなくても hwmon だけは更新する
                if !hw.is_empty() {
                    s.last_temp_ok = Instant::now();
                }
                s.temps = hw;
                s.temp_failures += 1;
                warn!(failures = s.temp_failures, error = %e, "ipmi temperature read failed");
                s.last_error = Some(e.to_string());
            }
        }
        refresh_state(&mut s, failure_limit);
        // 他ドメインの診断を消さないよう、健全な場合だけクリア
        if ok && is_healthy_state(s.state) {
            s.last_error = None;
        }
    }
}

/// 緊急温度チェック。閾値を超えたセンサーがあれば `(name, temp)`。
fn emergency_check(temps: &[TempReading], cpu_em: f32, pch_em: f32) -> Option<(String, f64)> {
    for (sensor_name, limit) in [("cpu_package", cpu_em), ("pch", pch_em)] {
        if let Some(t) = sensor::resolve(temps, sensor_name) {
            if t.celsius as f32 >= limit {
                return Some((sensor_name.to_string(), t.celsius));
            }
        }
    }
    None
}

/// 制御ループ。PWM への書き込みはここだけが行う
/// （`apply_mode` の IrmcAuto 即時解除を除く）。
#[allow(clippy::too_many_arguments)]
async fn control_loop<B: FanControlBackend>(
    backend: &B,
    shared: &RwLock<Shared>,
    ctrl: &Mutex<()>,
    mut shutdown: watch::Receiver<bool>,
    params: ControlParams,
    pi_params: PiParams,
    calib_table: Vec<CalibPoint>,
    calib_path: PathBuf,
    apply_interval: Duration,
    failure_limit: u32,
    cpu_emergency: f32,
    pch_emergency: f32,
    sensor_stale_secs: u64,
    fail_action: FailAction,
) {
    let mut limiter = RateLimiter::new(params);
    let mut pi = PiController::new(pi_params);
    let mut calib_job: Option<CalibJob> = None;
    let mut seen_gen = 0u64;
    let mut seen_scope = params.pwm_scope;
    let mut ticks_since_write = 0u32;
    let mut missing_sensors: Vec<String> = Vec::new();
    let mut emergency_active = false;
    let mut tick = interval(apply_interval);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = tick.tick() => {}
            // sender が send なしで drop された場合も busy-loop
            // にならないよう Err を終了扱いにする
            r = shutdown.changed() => {
                if r.is_err() {
                    return;
                }
            }
        }
        if *shutdown.borrow() {
            return;
        }
        // PWM 操作は apply_mode の即時解除と直列化する
        let _ctrl = ctrl.lock().await;
        let (
            mode,
            gen,
            temps,
            fans,
            curves,
            applied_pwm,
            clear_pending,
            sensor_stale,
            zero_rpm,
            scope,
            calib_state,
        ) = {
            let s = shared.read().await;
            (
                s.mode.clone(),
                s.mode_generation,
                s.temps.clone(),
                s.fans.clone(),
                s.curves.clone(),
                s.pwm,
                s.clear_pending,
                s.last_temp_ok.elapsed() > Duration::from_secs(sensor_stale_secs),
                s.zero_rpm_detected,
                s.pwm_scope,
                s.calibration.clone(),
            )
        };
        {
            // 陳腐化フラグを refresh_state の失敗ドメインへ反映
            let mut s = shared.write().await;
            if s.sensor_stale != sensor_stale {
                s.sensor_stale = sensor_stale;
                refresh_state(&mut s, failure_limit);
            }
        }
        if gen != seen_gen {
            limiter.reset();
            // 現在適用中の値から変化を継続する
            // （モード変更直後に目標へ直行するのを防ぐ）
            if let Some(p) = applied_pwm {
                limiter.prime(p);
            }
            // TargetRpm 突入時は PI を初期化する。キャリブレーション表
            // があれば目標 RPM から推定 PWM を初期値にして収束を速める。
            // なければ現在の適用値から滑らかに開始する。
            if let Mode::TargetRpm { fan, rpm } = &mode {
                let seed =
                    estimate_pwm(&calib_table, fan, *rpm).or_else(|| applied_pwm.map(|p| p as f32));
                match seed {
                    Some(p) => pi.prime(p),
                    None => pi.prime(params.min_pwm as f32),
                }
            }
            ticks_since_write = 0;
            seen_gen = gen;
        }
        // キャリブレーション要求の検出・中断処理:
        // shared.calibration が立っていれば（mode == Calibrate で）
        // ジョブを起こす。モードが切り替わったり calibration が
        // クリアされたらジョブを捨てる（中断）。
        if mode == Mode::Calibrate && calib_state.as_ref().is_some_and(|c| c.active) {
            if calib_job.is_none() {
                calib_job = Some(CalibJob::new(&params));
            }
        } else {
            calib_job = None;
        }
        // スコープ切替時は、強制中の値を新スコープで直ちに書き直す。
        // W コマンドはスコープ外のファンの強制状態を変更しないため、
        // all→chassis で PSU に残った強制を解くには先に全スロットの
        // override 解除が必要（実機検証済み: 解除なしでは PSU が
        // 旧強制値のまま固着する）。IrmcAuto/pwm=None なら書く
        // ものがないので何もしない。
        // 緊急・フェイル経路は毎 tick `scope` を読むため自動追従する。
        if scope != seen_scope {
            seen_scope = scope;
            if let Some(p) = applied_pwm {
                info!(
                    scope = scope.as_str(),
                    pwm = p,
                    "pwm scope changed; re-writing"
                );
                let clear_ok = match backend.clear_override().await {
                    Ok(()) => true,
                    Err(e) => {
                        warn!(error = %e, "scope change: clear failed; out-of-scope fans may stay forced");
                        let mut s = shared.write().await;
                        s.write_failures += 1;
                        s.last_error = Some(e.to_string());
                        false
                    }
                };
                write_pwm(backend, shared, p, failure_limit, scope).await;
                if !clear_ok {
                    // write_pwm の成功で clear_pending が降りても、
                    // スコープ外ファンの強制解除は未確認のまま残す
                    shared.write().await.clear_pending = true;
                }
                ticks_since_write = 0;
            }
        }

        // override 解除の未達を、強制していないモードでも再試行する。
        // 起動時 clear やスコープ遷移時 clear の失敗で残った
        // スコープ外の強制（PSU 固着など）を、Auto 以外でも
        // 放置しない。強制中（applied_pwm=Some）は解除しない
        // — 冷却のための強制を一瞬でも落とさない。
        if clear_pending && applied_pwm.is_none() && !matches!(mode, Mode::IrmcAuto) {
            match backend.clear_override().await {
                Ok(()) => {
                    let mut s = shared.write().await;
                    s.clear_pending = false;
                    s.write_failures = 0;
                    refresh_state(&mut s, failure_limit);
                    if is_healthy_state(s.state) {
                        s.last_error = None;
                    }
                    info!("override cleared (retry)");
                }
                Err(e) => {
                    let mut s = shared.write().await;
                    s.write_failures += 1;
                    s.last_error = Some(e.to_string());
                    warn!(error = %e, "clear_override retry failed");
                }
            }
        }

        // 緊急温度: モードに関わらず 100% 強制が最優先
        if let Some((sensor_name, t)) = emergency_check(&temps, cpu_emergency, pch_emergency) {
            emergency_active = true;
            warn!(sensor = %sensor_name, temp = t, "emergency temperature; forcing 100% pwm");
            write_pwm(backend, shared, EMERGENCY_PWM, failure_limit, scope).await;
            let mut s = shared.write().await;
            s.state = DaemonState::Failsafe;
            s.last_error = Some(format!("emergency: {sensor_name} {t:.1}C"));
            continue;
        }
        if emergency_active {
            // 緊急解除後の復帰。Auto モードでは 100% 強制が残らない
            // よう解除して Monitoring へ戻す。FixedPwm は次の
            // 書き込みで p に即戻り、Curve は 100% 起点で
            // limiter 経由の滑らかな降下になる。いずれも
            // Failsafe ラッチはここで解除する（refresh_state は
            // Failsafe を上書きしないので、先に正常状態へ
            // 戻してから再評価）
            emergency_active = false;
            info!("emergency cleared; restoring normal control");
            if mode == Mode::IrmcAuto {
                match backend.clear_override().await {
                    Ok(()) => {
                        let mut s = shared.write().await;
                        s.pwm = None;
                        s.clear_pending = false;
                        s.write_failures = 0;
                        s.state = normal_state(&s.mode);
                        refresh_state(&mut s, failure_limit);
                        if is_healthy_state(s.state) {
                            s.last_error = None;
                        }
                    }
                    Err(e) => {
                        let mut s = shared.write().await;
                        s.clear_pending = true;
                        s.write_failures += 1;
                        s.last_error = Some(e.to_string());
                        s.state = DaemonState::Degraded;
                        error!(error = %e, "post-emergency clear failed; will retry");
                    }
                }
            } else {
                limiter.prime(EMERGENCY_PWM);
                let mut s = shared.write().await;
                s.state = normal_state(&s.mode);
                refresh_state(&mut s, failure_limit);
                if is_healthy_state(s.state) {
                    s.last_error = None;
                }
            }
        }

        // 監視系フェイル（温度データ陳腐化・0 RPM）。条件が
        // 解消するまで通常モードの制御を停止し、設定された
        // フェイルアクションを適用する
        if sensor_stale || zero_rpm {
            let reason = if sensor_stale && zero_rpm {
                "sensor data stale + fan at 0 RPM"
            } else if sensor_stale {
                "sensor data stale"
            } else {
                "fan at 0 RPM"
            };
            run_fail_action(backend, shared, fail_action, reason, failure_limit, scope).await;
            continue;
        }

        match mode {
            Mode::IrmcAuto => {
                // clear が未達なら再試行する（pwm=None 表示のまま
                // 強制が残る状態を放置しない）
                if clear_pending {
                    match backend.clear_override().await {
                        Ok(()) => {
                            let mut s = shared.write().await;
                            s.clear_pending = false;
                            s.pwm = None;
                            s.write_failures = 0;
                            refresh_state(&mut s, failure_limit);
                            if is_healthy_state(s.state) {
                                s.last_error = None;
                            }
                            info!("override cleared (retry)");
                        }
                        Err(e) => {
                            let mut s = shared.write().await;
                            s.write_failures += 1;
                            s.last_error = Some(e.to_string());
                            s.state = DaemonState::Degraded;
                            warn!(error = %e, "clear_override retry failed");
                        }
                    }
                }
            }
            Mode::FixedPwm(p) => {
                // 外部要因で解除される可能性に備え周期再アサート。
                // ただし値が変わった tick で即時書き込み、変わらなければ
                // REASSERT_TICKS ごとに再送する。
                ticks_since_write += 1;
                if applied_pwm != Some(p) || ticks_since_write >= REASSERT_TICKS {
                    write_pwm(backend, shared, p, failure_limit, scope).await;
                    ticks_since_write = 0;
                }
            }
            Mode::Curve => {
                // 初回ポーリング完了前（temps 空）に誤って
                // 「センサー全滅」判定しないようスキップする
                if temps.is_empty() {
                    continue;
                }
                let (target, missing) = curve_demand(&curves, &temps);
                // 新たに解決不能になったセンサーだけ warn（ログスパム防止）
                for name in missing.iter().filter(|n| !missing_sensors.contains(n)) {
                    warn!(sensor = %name, "curve sensor not readable");
                }
                missing_sensors = missing;
                match target {
                    None => {
                        // 参照センサーが全滅 → 独自制御は捨てて iRMC へ戻す。
                        // clear 失敗時は pwm を Some のまま残し次 tick で再試行する。
                        // （clear_pending だけ立っている遷移経路も拾う）
                        let needs_clear = {
                            let s = shared.read().await;
                            s.pwm.is_some() || s.clear_pending
                        };
                        if needs_clear {
                            warn!("no curve sensors readable; clearing override (iRMC auto)");
                            match backend.clear_override().await {
                                Ok(()) => {
                                    let mut s = shared.write().await;
                                    s.pwm = None;
                                    s.clear_pending = false;
                                    s.write_failures = 0;
                                }
                                Err(e) => {
                                    let mut s = shared.write().await;
                                    s.write_failures += 1;
                                    s.clear_pending = true;
                                    s.last_error = Some(e.to_string());
                                    error!(error = %e, "clear_override failed; will retry");
                                }
                            }
                        }
                        // センサー全滅は refresh_state の失敗ドメイン
                        // として管理（直書きだとポーリング成功ごとに
                        // 状態が振動する）
                        let mut s = shared.write().await;
                        s.curve_sensors_missing = true;
                        s.last_error = Some("no curve sensors readable".into());
                        refresh_state(&mut s, failure_limit);
                    }
                    Some(target) => {
                        shared.write().await.curve_sensors_missing = false;
                        match limiter.next(target) {
                            Some(p) => {
                                write_pwm(backend, shared, p, failure_limit, scope).await;
                                ticks_since_write = 0;
                            }
                            None => {
                                ticks_since_write += 1;
                                if ticks_since_write >= REASSERT_TICKS {
                                    if let Some(p) = limiter.current() {
                                        write_pwm(backend, shared, p, failure_limit, scope).await;
                                    }
                                    ticks_since_write = 0;
                                }
                            }
                        }
                    }
                }
            }
            Mode::TargetRpm { fan, rpm } => {
                match fan_rpm(&fans, &fan) {
                    Some(measured) if measured > 0 => {
                        shared.write().await.target_fan_missing = false;
                        let p = pi.next(measured as f32, rpm as f32);
                        ticks_since_write += 1;
                        if applied_pwm != Some(p) || ticks_since_write >= REASSERT_TICKS {
                            write_pwm(backend, shared, p, failure_limit, scope).await;
                            ticks_since_write = 0;
                        }
                    }
                    _ => {
                        // 参照ファンが読めない → 独自制御を捨てて
                        // iRMC Auto へ退避（Curve のセンサー全滅と同じ
                        // フェイルセーフ。制御ループは毎 tick 再評価する）
                        let needs_clear = {
                            let s = shared.read().await;
                            s.pwm.is_some() || s.clear_pending
                        };
                        if needs_clear {
                            warn!(fan = %fan, "target rpm: reference fan not readable; clearing override");
                            match backend.clear_override().await {
                                Ok(()) => {
                                    let mut s = shared.write().await;
                                    s.pwm = None;
                                    s.clear_pending = false;
                                    s.write_failures = 0;
                                }
                                Err(e) => {
                                    let mut s = shared.write().await;
                                    s.write_failures += 1;
                                    s.clear_pending = true;
                                    s.last_error = Some(e.to_string());
                                    error!(error = %e, "clear_override failed; will retry");
                                }
                            }
                        }
                        let mut s = shared.write().await;
                        s.target_fan_missing = true;
                        s.last_error = Some(format!("reference fan '{fan}' not readable"));
                        refresh_state(&mut s, failure_limit);
                    }
                }
            }
            Mode::Calibrate => {
                match calib_job.as_mut() {
                    Some(job) => match job.tick(&fans, Instant::now()) {
                        CalibStep::Write(p) => {
                            write_pwm(backend, shared, p, failure_limit, scope).await;
                            ticks_since_write = 0;
                            shared.write().await.calibration = Some(job.status());
                        }
                        CalibStep::Wait => {
                            shared.write().await.calibration = Some(job.status());
                        }
                        CalibStep::Done(points) => {
                            let saved = persist_calibration(&calib_path, &points);
                            let mut s = shared.write().await;
                            match &saved {
                                Ok(()) => {
                                    info!(path = %calib_path.display(), "calibration written")
                                }
                                Err(e) => {
                                    warn!(error = %e, "calibration save failed")
                                }
                            }
                            let n = points.len();
                            s.calibration = Some(CalibStatus {
                                active: false,
                                current_pwm: None,
                                step: n,
                                total: n,
                                points,
                                result: Some(match saved {
                                    Ok(()) => {
                                        format!("saved to {}", calib_path.display())
                                    }
                                    Err(e) => format!("measured ok; save failed: {e:#}"),
                                }),
                            });
                            // 開始前のモードへ戻す
                            s.mode = s.calib_resume_mode.clone();
                            s.mode_generation += 1;
                            calib_job = None;
                        }
                    },
                    None => {
                        // calibration 状態なしの Calibrate モード
                        // （外部直接指定等）→ 安全側の Auto へ退避
                        let mut s = shared.write().await;
                        s.mode = Mode::IrmcAuto;
                        s.mode_generation += 1;
                    }
                }
            }
        }
    }
}

/// 監視系フェイルのアクションを実行する。
/// 条件が続く間は制御ループが毎 tick 呼ぶ（冪等）。
async fn run_fail_action<B: FanControlBackend>(
    backend: &B,
    shared: &RwLock<Shared>,
    action: FailAction,
    reason: &str,
    failure_limit: u32,
    scope: PwmScope,
) {
    match action {
        FailAction::IrmcAuto => {
            let needs_clear = {
                let s = shared.read().await;
                s.pwm.is_some() || s.clear_pending
            };
            if needs_clear {
                warn!(reason, "fail action: clearing override (iRMC auto)");
                match backend.clear_override().await {
                    Ok(()) => {
                        let mut s = shared.write().await;
                        s.pwm = None;
                        s.clear_pending = false;
                        s.write_failures = 0;
                    }
                    Err(e) => {
                        let mut s = shared.write().await;
                        s.clear_pending = true;
                        s.write_failures += 1;
                        s.last_error = Some(e.to_string());
                        error!(error = %e, "fail action clear failed; will retry");
                    }
                }
            }
            let mut s = shared.write().await;
            if s.last_error.is_none() {
                s.last_error = Some(format!("failsafe: {reason}"));
            }
            refresh_state(&mut s, failure_limit);
        }
        FailAction::FullSpeed => {
            warn!(reason, "fail action: forcing 100% pwm");
            write_pwm(backend, shared, EMERGENCY_PWM, failure_limit, scope).await;
            let mut s = shared.write().await;
            if s.last_error.is_none() {
                s.last_error = Some(format!("failsafe: {reason}"));
            }
        }
    }
}

/// 全カーブを現在温度で評価し、最大要求 PWM と
/// 解決不能だったセンサー名の一覧を返す。
/// 1つも解決できなければ `None`。
///
/// 一部のセンサーが解決不能でも、解決できたカーブの最大値で
/// 制御を継続する（解決不能なカーブは要求に参加しない。
/// 「全滅」したときだけフェイルセーフへ退避する）。
fn curve_demand(curves: &[Curve], temps: &[TempReading]) -> (Option<u8>, Vec<String>) {
    let mut demand = None;
    let mut missing = Vec::new();
    for c in curves {
        match sensor::resolve(temps, &c.sensor) {
            Some(t) => {
                let p = c.eval(t.celsius as f32).round().clamp(0.0, 100.0) as u8;
                demand = Some(demand.map_or(p, |d: u8| d.max(p)));
            }
            None => missing.push(c.sensor.clone()),
        }
    }
    (demand, missing)
}

/// ファン名から現在 RPM を引く。名前照合は前後空白と
/// 大文字小文字を緩和する（SDR 名 "FAN CPU" と config /
/// ユーザー入力の差を吸収）。OK 以外のステータスのファンは
/// 読み取り不能扱いにする。
fn fan_rpm(fans: &[FanReading], name: &str) -> Option<u32> {
    use pmgfan_core::fan::FanStatus;
    let want = name.trim();
    let f = fans
        .iter()
        .find(|f| f.name.trim().eq_ignore_ascii_case(want) && f.status == FanStatus::Ok)
        .or_else(|| {
            fans.iter()
                .find(|f| f.name.trim().eq_ignore_ascii_case(want))
        })?;
    f.rpm.filter(|r| *r > 0)
}

/// キャリブレーション表から目標 RPM に対応する PWM を線形補間で
/// 推定する（PI コントローラの初期値シード用）。参照ファンの
/// 計測列が無い・目標がレンジ外なら端点クランプまたは None。
fn estimate_pwm(table: &[CalibPoint], fan: &str, target_rpm: u32) -> Option<f32> {
    let mut pts: Vec<(f32, f32)> = table
        .iter()
        .filter_map(|p| {
            p.rpm
                .iter()
                .find(|(n, _)| n.trim().eq_ignore_ascii_case(fan.trim()))
                .map(|(_, r)| (*r as f32, p.pwm as f32))
        })
        .collect();
    if pts.is_empty() {
        return None;
    }
    pts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let t = target_rpm as f32;
    let first = *pts.first().unwrap();
    let last = *pts.last().unwrap();
    if t <= first.0 {
        return Some(first.1);
    }
    if t >= last.0 {
        return Some(last.1);
    }
    for w in pts.windows(2) {
        let (r0, p0) = w[0];
        let (r1, p1) = w[1];
        if t >= r0 && t <= r1 && r1 > r0 {
            return Some(p0 + (p1 - p0) * (t - r0) / (r1 - r0));
        }
    }
    Some(last.1)
}

/// キャリブレーションの1レベル整定時間。
const CALIB_SETTLE: Duration = Duration::from_secs(8);
/// 1レベルのサンプリング窓。
const CALIB_SAMPLE: Duration = Duration::from_secs(4);
/// レベル刻み（`min_pwm` から 100% までこの刻みで掃く）。
const CALIB_STEP: u8 = 10;

#[derive(Debug, PartialEq)]
enum CalibPhase {
    /// PWM 書き換え後の収束待ち
    Settle,
    /// RPM サンプル収集中
    Sample,
}

/// キャリブレーションジョブ。制御ループが1 tick ずつ
/// `tick()` を呼んで PWM→RPM 特性を掃引する。
/// 緊急温度・センサーフェイル等の安全経路は制御ループ側で
/// 先に処理されるため、ここでは純粋に掃引だけを考える。
struct CalibJob {
    levels: Vec<u8>,
    idx: usize,
    /// 最初のレベル書き込みを出したか
    started: bool,
    phase: CalibPhase,
    phase_start: Instant,
    /// 現在レベルで集めた (fan名, rpm 列)
    samples: Vec<(String, Vec<u32>)>,
    points: Vec<CalibPoint>,
}

enum CalibStep {
    /// この PWM を書き込む
    Write(u8),
    /// 何もしない（整定/収集中）
    Wait,
    /// 全レベル完了
    Done(Vec<CalibPoint>),
}

impl CalibJob {
    fn new(params: &ControlParams) -> Self {
        let mut levels: Vec<u8> = (params.min_pwm..=100)
            .step_by(CALIB_STEP as usize)
            .collect();
        if levels.last() != Some(&100) {
            levels.push(100);
        }
        Self {
            levels,
            idx: 0,
            started: false,
            phase: CalibPhase::Settle,
            phase_start: Instant::now(),
            samples: Vec::new(),
            points: Vec::new(),
        }
    }

    fn status(&self) -> CalibStatus {
        CalibStatus {
            active: true,
            current_pwm: Some(self.levels[self.idx]),
            step: self.points.len(),
            total: self.levels.len(),
            points: self.points.clone(),
            result: None,
        }
    }

    fn tick(&mut self, fans: &[FanReading], now: Instant) -> CalibStep {
        // 最初の呼び出しで min レベルを書き込み、整定フェーズへ。
        if !self.started {
            self.started = true;
            self.phase = CalibPhase::Settle;
            self.phase_start = now;
            return CalibStep::Write(self.levels[0]);
        }
        match self.phase {
            CalibPhase::Settle => {
                if now.duration_since(self.phase_start) >= CALIB_SETTLE {
                    self.phase = CalibPhase::Sample;
                    self.phase_start = now;
                    self.samples.clear();
                }
                CalibStep::Wait
            }
            CalibPhase::Sample => {
                for f in fans {
                    if let Some(rpm) = f.rpm {
                        if rpm == 0 {
                            continue;
                        }
                        match self.samples.iter_mut().find(|(n, _)| *n == f.name) {
                            Some((_, v)) => v.push(rpm),
                            None => self.samples.push((f.name.clone(), vec![rpm])),
                        }
                    }
                }
                if now.duration_since(self.phase_start) >= CALIB_SAMPLE {
                    let rpm = self
                        .samples
                        .iter()
                        .map(|(n, v)| (n.clone(), median(v)))
                        .collect();
                    self.points.push(CalibPoint {
                        pwm: self.levels[self.idx],
                        rpm,
                    });
                    self.idx += 1;
                    if self.idx >= self.levels.len() {
                        return CalibStep::Done(std::mem::take(&mut self.points));
                    }
                    self.phase = CalibPhase::Settle;
                    self.phase_start = now;
                    return CalibStep::Write(self.levels[self.idx]);
                }
                CalibStep::Wait
            }
        }
    }
}

/// u32 列の中央値（偶数個なら中央2つの平均）。
fn median(v: &[u32]) -> u32 {
    if v.is_empty() {
        return 0;
    }
    let mut v = v.to_vec();
    v.sort_unstable();
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        ((v[n / 2 - 1] as u64 + v[n / 2] as u64) / 2) as u32
    }
}

/// キャリブレーション結果を TOML で保存する（tmp+rename で原子更新。
/// 他キーとの混在は無い全生成ファイルなので toml_edit は不要）。
pub fn persist_calibration(path: &Path, points: &[CalibPoint]) -> Result<()> {
    use pmgfan_core::protocol::{CalibFile, CalibFilePoint};

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let file = CalibFile {
        point: points
            .iter()
            .map(|p| CalibFilePoint {
                pwm: p.pwm,
                rpm: p.rpm.iter().cloned().collect(),
            })
            .collect(),
    };
    let text = toml::to_string_pretty(&file).context("cannot serialize calibration")?;
    let tmp = path.with_file_name(format!(
        ".{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "calibration.toml".into())
    ));
    std::fs::write(&tmp, text).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("cannot rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// キャリブレーション結果を読み込む。無い/壊れていれば空。
pub fn load_calibration(path: &Path) -> Vec<CalibPoint> {
    use pmgfan_core::protocol::CalibFile;

    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    match toml::from_str::<CalibFile>(&text) {
        Ok(f) => f
            .point
            .into_iter()
            .map(|p| CalibPoint {
                pwm: p.pwm,
                rpm: p.rpm.into_iter().collect(),
            })
            .collect(),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "ignoring unreadable calibration file");
            Vec::new()
        }
    }
}

async fn write_pwm<B: FanControlBackend>(
    backend: &B,
    shared: &RwLock<Shared>,
    pwm: u8,
    failure_limit: u32,
    scope: PwmScope,
) {
    match backend.set_pwm(scope, pwm).await {
        Ok(()) => {
            let mut s = shared.write().await;
            s.pwm = Some(pwm);
            // 強制状態が既知になったので解除未達フラグも降ろす
            s.clear_pending = false;
            s.write_failures = 0;
            refresh_state(&mut s, failure_limit);
            // 他ドメインの診断を消さないよう、健全な場合だけクリア
            if is_healthy_state(s.state) {
                s.last_error = None;
            }
        }
        Err(e) => {
            let mut s = shared.write().await;
            s.write_failures += 1;
            s.last_error = Some(e.to_string());
            if s.write_failures >= failure_limit {
                s.state = DaemonState::Degraded;
            }
            warn!(failures = s.write_failures, error = %e, "pwm write failed");
        }
    }
}

fn read_hwmon() -> Vec<TempReading> {
    hwmon::read_temperatures(Path::new(hwmon::HWMON_ROOT)).unwrap_or_default()
}

/// `run_dir/pmgfand.lock` を flock(LOCK_EX|LOCK_NB) で排他取得する。
/// 取得した File を保持している間ロックは有効。既に他プロセスが
/// 保持していればエラーにする。
fn acquire_instance_lock(run_dir: &Path) -> Result<std::fs::File> {
    // ディレクトリを作成した場合のみ 0750 + pmgfan グループを
    // 適用する（systemd 経由では unit の Group=pmgfan が用意する）
    ipc::prepare_runtime_dir(run_dir)
        .with_context(|| format!("cannot create runtime directory {}", run_dir.display()))?;
    let lock_path = run_dir.join("pmgfand.lock");
    let file = std::fs::File::create(&lock_path)
        .with_context(|| format!("cannot open {}", lock_path.display()))?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        bail!(
            "another pmgfand instance is running (lock: {})",
            lock_path.display()
        );
    }
    Ok(file)
}

/// モード変更の検証と共有状態への反映。
///
/// `FixedPwm`/`Curve` は制御ループが次 tick で適用する。
/// `IrmcAuto` は強制残存が危険なためここで即座に解除する。
pub async fn apply_mode<B: FanControlBackend>(
    backend: &B,
    shared: &RwLock<Shared>,
    ctrl: &Mutex<()>,
    params: &ControlParams,
    mode: &Mode,
) -> std::result::Result<(), String> {
    match mode {
        Mode::IrmcAuto => {
            // 制御ループの書き込みと直列化。ここが先なら次 tick で
            // Auto を見て書き込みを止め、後ならこの解除が最終状態になる。
            let _ctrl = ctrl.lock().await;
            if let Err(e) = backend.clear_override().await {
                let mut s = shared.write().await;
                s.write_failures += 1;
                s.clear_pending = true;
                s.last_error = Some(e.to_string());
                // 他の失敗経路と揃えて即座に可視化する
                // （Failsafe は次 tick で再評価される）
                s.state = DaemonState::Degraded;
                return Err(e.to_string());
            }
            let mut s = shared.write().await;
            s.mode = Mode::IrmcAuto;
            s.mode_generation += 1;
            s.pwm = None;
            s.clear_pending = false;
            s.calibration = None;
            s.write_failures = 0;
            // 他ドメインの Degraded/Failsafe を上書きしない
            if s.state != DaemonState::Degraded && s.state != DaemonState::Failsafe {
                s.state = DaemonState::Monitoring;
                s.last_error = None;
            }
            info!("mode -> irmc_auto");
            Ok(())
        }
        Mode::FixedPwm(p) => {
            if *p > params.max_pwm {
                return Err(format!(
                    "pwm {p}% is above configured max {}%",
                    params.max_pwm
                ));
            }
            // 下限は config の min_pwm が権威（30 未満も設定可能。
            // 実機では低 duty でもシャーシファンは回転継続を確認済み）
            let floor = params.min_pwm;
            if *p < floor {
                return Err(format!("pwm {p}% is below the allowed floor {floor}%"));
            }
            let mut s = shared.write().await;
            s.mode = mode.clone();
            s.mode_generation += 1;
            s.calibration = None;
            info!(pwm = p, "mode -> fixed_pwm");
            Ok(())
        }
        Mode::Curve => {
            if shared.read().await.curves.is_empty() {
                return Err("no fan curves configured".into());
            }
            let mut s = shared.write().await;
            s.mode = Mode::Curve;
            s.mode_generation += 1;
            s.calibration = None;
            info!("mode -> curve");
            Ok(())
        }
        Mode::TargetRpm { fan, rpm } => {
            if fan.trim().is_empty() {
                return Err("target_rpm requires a reference fan name".into());
            }
            if !(500..=20000).contains(rpm) {
                return Err(format!("target rpm {rpm} out of sane range 500..=20000"));
            }
            // ファン一覧が既知なら参照ファンの存在を検証する。
            // 起動直後（まだ SDR が読めていない）では受理して
            // 制御ループの target_fan_missing フェイル経路に任せる。
            {
                let s = shared.read().await;
                let known = !s.fans.is_empty();
                let found = s
                    .fans
                    .iter()
                    .any(|f| f.name.trim().eq_ignore_ascii_case(fan.trim()));
                if known && !found {
                    let names: Vec<_> = s.fans.iter().map(|f| f.name.clone()).collect();
                    return Err(format!(
                        "reference fan '{fan}' not found (known: {})",
                        names.join(", ")
                    ));
                }
            }
            let mut s = shared.write().await;
            s.mode = mode.clone();
            s.mode_generation += 1;
            s.calibration = None;
            s.target_fan_missing = false;
            info!(fan = %fan, rpm = rpm, "mode -> target_rpm");
            Ok(())
        }
        Mode::Calibrate => {
            // キャリブレーションは StartCalibration リクエスト経由で
            // 開始する（mode と calibration 状態を同時に立てる必要が
            // あるため、set_mode 経路では拒否する）
            Err("calibration is started via the calibrate request, not set_mode".into())
        }
    }
}

/// `Curve` を IPC のワイヤ表現へ変換する。
pub fn curve_to_spec(c: &Curve) -> CurveSpec {
    CurveSpec {
        sensor: c.sensor.clone(),
        points: c.points.iter().map(|p| (p.temp, p.pwm)).collect(),
    }
}

/// SetCurves 要求の検証。通れば `Curve` のリストに変換する。
///
/// `Curve::new` の検査（2点以上・温度厳密昇順・PWM 0..=100・
/// 有限値）に加えて、制御設定と矛盾しないことを確認する:
/// - 各点の PWM は `min_pwm` 以上（制御下限を下回る要求は
///   カーブとして意味を持たない）
/// - 最終点の PWM は `max_pwm` 以上（高温域で必ず最大風量へ
///   到達するカーブであることを保証する）
/// - 温度は 0..=150 ℃
pub fn validate_curves(
    specs: &[CurveSpec],
    params: &ControlParams,
) -> std::result::Result<Vec<Curve>, String> {
    if specs.is_empty() {
        return Err("at least one curve is required".into());
    }
    let mut out = Vec::with_capacity(specs.len());
    for spec in specs {
        if spec.sensor.trim().is_empty() {
            return Err("curve sensor name must not be empty".into());
        }
        let curve =
            Curve::new(spec.sensor.clone(), spec.points.clone()).map_err(|e| e.to_string())?;
        for (i, p) in curve.points.iter().enumerate() {
            if !(0.0..=150.0).contains(&p.temp) {
                return Err(format!(
                    "curve '{}': point {i} temp {} is out of range 0..=150C",
                    curve.sensor, p.temp
                ));
            }
            if p.pwm < params.min_pwm as f32 {
                return Err(format!(
                    "curve '{}': point {i} pwm {}% is below min_pwm {}%",
                    curve.sensor, p.pwm, params.min_pwm
                ));
            }
            if p.pwm > params.max_pwm as f32 {
                return Err(format!(
                    "curve '{}': point {i} pwm {}% is above max_pwm {}%",
                    curve.sensor, p.pwm, params.max_pwm
                ));
            }
        }
        let last = curve.points.last().expect("validated >= 2 points");
        if last.pwm < params.max_pwm as f32 {
            return Err(format!(
                "curve '{}': last point pwm {}% is below max_pwm {}% \
                 (the curve must reach full demand at high temperature)",
                curve.sensor, last.pwm, params.max_pwm
            ));
        }
        out.push(curve);
    }
    Ok(out)
}

/// 検証済みカーブを設定ファイルの `[[curve]]` セクションへ
/// 書き戻す。`toml_edit` で他のキーとコメントを保持する。
/// 直接 `write` せず tmp + rename で中途半端なファイルを残さない。
pub fn persist_curves(path: &Path, curves: &[Curve]) -> Result<()> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("cannot parse {}", path.display()))?;
    doc.remove("curve");
    let mut arr = toml_edit::ArrayOfTables::new();
    for c in curves {
        let mut t = toml_edit::Table::new();
        t["sensor"] = toml_edit::value(c.sensor.clone());
        let mut pts = toml_edit::Array::new();
        for p in &c.points {
            let mut pair = toml_edit::Array::new();
            pair.push(p.temp as f64);
            pair.push(p.pwm as f64);
            pts.push(pair);
        }
        t["points"] = toml_edit::Item::Value(toml_edit::Value::Array(pts));
        arr.push(t);
    }
    doc["curve"] = toml_edit::Item::ArrayOfTables(arr);
    persist_doc(path, &doc)
}

/// 実行時 PWM スコープを設定ファイルの `[control] pwm_scope`
/// へ書き戻す。`persist_curves` と同じく tmp + rename で原子的。
pub fn persist_scope(path: &Path, scope: PwmScope) -> Result<()> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut doc = text
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("cannot parse {}", path.display()))?;
    // 読み取り index は存在しないキーで panic するため get で確認する
    if doc.get("control").is_none_or(|t| !t.is_table()) {
        doc["control"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    doc["control"]["pwm_scope"] = toml_edit::value(scope.as_str());
    persist_doc(path, &doc)
}

/// TOML ドキュメントを tmp + rename で原子的に書き戻す。
fn persist_doc(path: &Path, doc: &toml_edit::DocumentMut) -> Result<()> {
    let tmp = path.with_file_name(format!(
        ".{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "config.toml".into())
    ));
    std::fs::write(&tmp, doc.to_string())
        .with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("cannot rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// `NOTIFY_SOCKET` があれば sd_notify を送る。
pub async fn sd_notify(msg: &str) {
    let Ok(sock) = std::env::var("NOTIFY_SOCKET") else {
        return;
    };
    let Ok(dgram) = UnixDatagram::unbound() else {
        return;
    };
    let _ = dgram.send_to(msg.as_bytes(), &sock).await;
}

/// systemd が WATCHDOG_USEC を設定していれば、その半周期で
/// `WATCHDOG=1` を送り続けるタスクを起動する。
///
/// ただしファンポーリングの鮮度が watchdog 周期内に保たれて
/// いるときだけ送る。ポーリングがハング（ipmitool wedged 等）
/// しているときはキックを止め、systemd に検出・再起動させる
/// （docs/04「プロセスハングまで考慮」）。
fn spawn_watchdog(shared: Arc<RwLock<Shared>>) {
    let Ok(usec) = std::env::var("WATCHDOG_USEC") else {
        return;
    };
    let Ok(usec) = usec.parse::<u64>() else {
        return;
    };
    if usec == 0 {
        return;
    }
    let period = Duration::from_micros(usec);
    // WATCHDOG_USEC が極小だと half が 0 になり interval が
    // panic するのを防ぐ
    let half = (period / 2).max(Duration::from_millis(100));
    tokio::spawn(async move {
        let mut t = interval(half);
        loop {
            t.tick().await;
            let fresh = shared.read().await.last_poll_ok.elapsed() < period;
            if fresh {
                sd_notify("WATCHDOG=1").await;
            } else {
                warn!("fan polling stale; suppressing watchdog kick");
            }
        }
    });
    info!(watchdog_usec = usec, "systemd watchdog enabled");
}

#[cfg(test)]
mod tests {
    use super::*;
    use pmgfan_core::fan::{FanReading, FanStatus};
    use pmgfan_core::sensor::TempReading;
    use pmgfan_ipmi::backend::{IpmiError, PwmSlot, Result as IpmiResult};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// モックバックエンド。呼び出し回数と現在の強制値を記録する。
    struct MockBackend {
        model: String,
        pwm: Mutex<Option<u8>>,
        /// 最後に書かれたスコープ（clear で None に戻る）
        scope: Mutex<Option<PwmScope>>,
        clear_calls: AtomicUsize,
        fail_clear: AtomicBool,
        fail_write: AtomicBool,
        temps: Vec<TempReading>,
        fans: Vec<FanReading>,
    }

    impl MockBackend {
        fn new() -> Self {
            Self {
                model: "PRIMERGY TX1320 M4".into(),
                pwm: Mutex::new(None),
                scope: Mutex::new(None),
                clear_calls: AtomicUsize::new(0),
                fail_clear: AtomicBool::new(false),
                fail_write: AtomicBool::new(false),
                temps: Vec::new(),
                fans: Vec::new(),
            }
        }
    }

    impl FanControlBackend for MockBackend {
        fn model_name(&self) -> impl std::future::Future<Output = IpmiResult<String>> + Send {
            async move { Ok(self.model.clone()) }
        }
        fn fans(&self) -> impl std::future::Future<Output = IpmiResult<Vec<FanReading>>> + Send {
            async move { Ok(self.fans.clone()) }
        }
        fn temperatures(
            &self,
        ) -> impl std::future::Future<Output = IpmiResult<Vec<TempReading>>> + Send {
            async move { Ok(self.temps.clone()) }
        }
        fn set_pwm(
            &self,
            scope: PwmScope,
            pwm: u8,
        ) -> impl std::future::Future<Output = IpmiResult<()>> + Send {
            async move {
                if self.fail_write.load(Ordering::SeqCst) {
                    return Err(IpmiError::Parse("mock write failure".into()));
                }
                *self.scope.lock().await = Some(scope);
                *self.pwm.lock().await = Some(pwm);
                Ok(())
            }
        }
        fn clear_override(&self) -> impl std::future::Future<Output = IpmiResult<()>> + Send {
            async move {
                self.clear_calls.fetch_add(1, Ordering::SeqCst);
                if self.fail_clear.load(Ordering::SeqCst) {
                    return Err(IpmiError::Parse("mock clear failure".into()));
                }
                *self.pwm.lock().await = None;
                *self.scope.lock().await = None;
                Ok(())
            }
        }
        fn read_override_slots(
            &self,
            _indices: &[u8],
        ) -> impl std::future::Future<Output = IpmiResult<Vec<PwmSlot>>> + Send {
            async move { Ok(Vec::new()) }
        }
    }

    fn params() -> ControlParams {
        ControlParams {
            min_pwm: 30,
            max_pwm: 100,
            pwm_scope: PwmScope::All,
            step_up: 20,
            step_down: 5,
            down_hysteresis: 5,
        }
    }

    fn test_pi() -> PiParams {
        PiParams {
            kp: 0.003,
            ki: 0.0001,
            deadband_rpm: 75.0,
            min_pwm: 30,
            max_pwm: 100,
        }
    }

    fn shared_with_mode(mode: Mode) -> RwLock<Shared> {
        RwLock::new(Shared::new(mode, vec![], PwmScope::All))
    }

    #[tokio::test]
    async fn apply_mode_fixed_pwm_validates_floor() {
        let b = MockBackend::new();
        let s = shared_with_mode(Mode::IrmcAuto);
        let c = Mutex::new(());
        // 設定下限未満は拒否
        assert!(apply_mode(&b, &s, &c, &params(), &Mode::FixedPwm(10))
            .await
            .is_err());
        // 上限超過も拒否
        assert!(apply_mode(&b, &s, &c, &params(), &Mode::FixedPwm(101))
            .await
            .is_err());
        // 範囲内は受け付けてモード反映
        assert!(apply_mode(&b, &s, &c, &params(), &Mode::FixedPwm(40))
            .await
            .is_ok());
        let s = s.read().await;
        assert_eq!(s.mode, Mode::FixedPwm(40));
        assert_eq!(s.mode_generation, 1);
    }

    #[tokio::test]
    async fn apply_mode_auto_clears_and_updates_state() {
        let b = MockBackend::new();
        let s = shared_with_mode(Mode::Curve);
        {
            let mut w = s.write().await;
            w.pwm = Some(40);
            w.state = DaemonState::Controlling;
        }
        let c = Mutex::new(());
        apply_mode(&b, &s, &c, &params(), &Mode::IrmcAuto)
            .await
            .unwrap();
        let s = s.read().await;
        assert_eq!(s.mode, Mode::IrmcAuto);
        assert_eq!(s.state, DaemonState::Monitoring);
        assert_eq!(s.pwm, None);
        assert_eq!(b.clear_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn apply_mode_auto_failure_is_reported() {
        let b = MockBackend::new();
        b.fail_clear.store(true, Ordering::SeqCst);
        let s = shared_with_mode(Mode::Curve);
        s.write().await.pwm = Some(40);
        let c = Mutex::new(());
        assert!(apply_mode(&b, &s, &c, &params(), &Mode::IrmcAuto)
            .await
            .is_err());
        let s = s.read().await;
        // 失敗時はモード・pwm を変えない（診断だけ残す）。
        // 解除未達を記録して制御ループのリトライに委ねる
        assert_eq!(s.mode, Mode::Curve);
        assert_eq!(s.pwm, Some(40));
        assert!(s.clear_pending);
        assert_eq!(s.write_failures, 1);
        assert!(s.last_error.is_some());
    }

    #[tokio::test]
    async fn write_pwm_clears_pending_flag() {
        let b = MockBackend::new();
        let s = shared_with_mode(Mode::FixedPwm(40));
        s.write().await.clear_pending = true;
        write_pwm(&b, &s, 40, 3, PwmScope::Chassis).await;
        assert_eq!(*b.scope.lock().await, Some(PwmScope::Chassis));
        let s = s.read().await;
        assert_eq!(s.pwm, Some(40));
        assert!(!s.clear_pending);
        assert_eq!(s.state, DaemonState::Controlling);
    }

    #[test]
    fn refresh_state_tracks_all_failure_domains() {
        // 失敗ドメインのいずれかが閾値超過なら Degraded、
        // 全部健全になって初めて normal に戻る（フラッピング防止）
        let mut s = Shared::new(Mode::Curve, vec![], PwmScope::All);
        s.state = DaemonState::Degraded;
        s.temp_failures = 3;
        s.fan_failures = 0;
        refresh_state(&mut s, 3);
        assert_eq!(s.state, DaemonState::Degraded); // まだ閾値超過
        s.temp_failures = 0;
        s.clear_pending = true;
        refresh_state(&mut s, 3);
        assert_eq!(s.state, DaemonState::Degraded); // clear 未達も Degraded
        s.clear_pending = false;
        refresh_state(&mut s, 3);
        assert_eq!(s.state, DaemonState::Controlling); // 全解消で回復
                                                       // Failsafe（緊急温度）はここでは上書きしない
        s.state = DaemonState::Failsafe;
        s.fan_failures = 0;
        refresh_state(&mut s, 3);
        assert_eq!(s.state, DaemonState::Failsafe);
    }

    #[tokio::test]
    async fn apply_mode_curve_requires_curves() {
        let b = MockBackend::new();
        let s = shared_with_mode(Mode::IrmcAuto);
        let c = Mutex::new(());
        assert!(apply_mode(&b, &s, &c, &params(), &Mode::Curve)
            .await
            .is_err());
    }

    #[test]
    fn emergency_check_triggers_on_threshold() {
        let temps = vec![
            TempReading {
                chip: "ipmi".into(),
                label: "CPU".into(),
                celsius: 91.0,
            },
            TempReading {
                chip: "ipmi".into(),
                label: "PCH".into(),
                celsius: 50.0,
            },
        ];
        assert!(emergency_check(&temps, 90.0, 95.0).is_some());
        assert!(emergency_check(&temps, 95.0, 95.0).is_none());
        // センサー自体が無ければ発火しない
        assert!(emergency_check(&[], 90.0, 95.0).is_none());
    }

    /// 緊急温度復帰の回帰テスト。`control_loop` を短命間隔で
    /// 実際に回し、Failsafe 発動→温度正常化→各モードの
    /// 期待状態への復帰を確認する。
    async fn run_emergency_cycle(
        mode: Mode,
    ) -> (
        Arc<RwLock<Shared>>,
        Arc<MockBackend>,
        tokio::sync::watch::Sender<bool>,
    ) {
        let b = Arc::new(MockBackend::new());
        let s = Arc::new(RwLock::new(Shared::new(mode, vec![], PwmScope::All)));
        let c = Arc::new(Mutex::new(()));
        let (tx, rx) = watch::channel(false);
        let (bb, ss, cc) = (Arc::clone(&b), Arc::clone(&s), Arc::clone(&c));
        let task = tokio::spawn(async move {
            control_loop(
                bb.as_ref(),
                ss.as_ref(),
                cc.as_ref(),
                rx,
                params(),
                test_pi(),
                Vec::new(),
                std::env::temp_dir().join("pmgfan-test-calib.toml"),
                Duration::from_millis(10),
                3,
                90.0,
                95.0,
                10,
                FailAction::IrmcAuto,
            )
            .await
        });
        // 緊急温度を注入
        s.write().await.temps = vec![TempReading {
            chip: "ipmi".into(),
            label: "CPU".into(),
            celsius: 95.0,
        }];
        tokio::time::sleep(Duration::from_millis(80)).await;
        {
            let st = s.read().await;
            assert_eq!(st.state, DaemonState::Failsafe);
            assert_eq!(st.pwm, Some(EMERGENCY_PWM));
        }
        // 温度正常化
        s.write().await.temps = vec![TempReading {
            chip: "ipmi".into(),
            label: "CPU".into(),
            celsius: 40.0,
        }];
        tokio::time::sleep(Duration::from_millis(120)).await;
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        (s, b, tx)
    }

    #[tokio::test]
    async fn failsafe_recovers_to_controlling_in_fixed_pwm() {
        // Failsafe がラッチされず、通常制御（固定 PWM）へ戻ること
        let (s, _b, _tx) = run_emergency_cycle(Mode::FixedPwm(50)).await;
        let st = s.read().await;
        assert_eq!(st.state, DaemonState::Controlling);
        assert_eq!(st.pwm, Some(50));
    }

    #[tokio::test]
    async fn failsafe_recovers_to_monitoring_in_auto() {
        // Auto モードでは 100% 強制が残らず解除・Monitoring へ戻る
        let (s, b, _tx) = run_emergency_cycle(Mode::IrmcAuto).await;
        let st = s.read().await;
        assert_eq!(st.state, DaemonState::Monitoring);
        assert_eq!(st.pwm, None);
        assert!(b.clear_calls.load(Ordering::SeqCst) >= 1);
    }

    /// センサー陳腐化フェイルの回帰テスト。last_temp_ok を
    /// 過去にして stale 発火 → fail action → 復帰まで確認。
    #[tokio::test]
    async fn sensor_stale_triggers_irmc_auto_and_recovers() {
        let b = Arc::new(MockBackend::new());
        let s = Arc::new(RwLock::new(Shared::new(
            Mode::FixedPwm(50),
            vec![],
            PwmScope::All,
        )));
        let c = Arc::new(Mutex::new(()));
        let (tx, rx) = watch::channel(false);
        let (bb, ss, cc) = (Arc::clone(&b), Arc::clone(&s), Arc::clone(&c));
        let task = tokio::spawn(async move {
            control_loop(
                bb.as_ref(),
                ss.as_ref(),
                cc.as_ref(),
                rx,
                params(),
                test_pi(),
                Vec::new(),
                std::env::temp_dir().join("pmgfan-test-calib.toml"),
                Duration::from_millis(10),
                3,
                90.0,
                95.0,
                1,
                FailAction::IrmcAuto,
            )
            .await
        });
        // 温度データが古いまま → stale 発火
        {
            let mut w = s.write().await;
            w.temps = vec![TempReading {
                chip: "ipmi".into(),
                label: "CPU".into(),
                celsius: 40.0,
            }];
            w.last_temp_ok = Instant::now() - Duration::from_secs(30);
            w.pwm = Some(50);
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
        {
            let st = s.read().await;
            assert_eq!(st.state, DaemonState::Degraded);
            assert!(st.sensor_stale);
            // irmc-auto fail action: override が解除される
            assert_eq!(st.pwm, None);
        }
        assert!(b.clear_calls.load(Ordering::SeqCst) >= 1);
        // 温度が新鮮に戻る → 通常制御（FixedPwm）へ復帰
        s.write().await.last_temp_ok = Instant::now();
        tokio::time::sleep(Duration::from_millis(80)).await;
        {
            let st = s.read().await;
            assert_eq!(st.state, DaemonState::Controlling);
            assert_eq!(st.pwm, Some(50));
        }
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
    }

    /// fail_action = "full-speed" の場合 100% 強制になる
    #[tokio::test]
    async fn sensor_stale_full_speed_action() {
        let b = Arc::new(MockBackend::new());
        let s = Arc::new(RwLock::new(Shared::new(
            Mode::FixedPwm(50),
            vec![],
            PwmScope::All,
        )));
        let c = Arc::new(Mutex::new(()));
        let (tx, rx) = watch::channel(false);
        let (bb, ss, cc) = (Arc::clone(&b), Arc::clone(&s), Arc::clone(&c));
        let task = tokio::spawn(async move {
            control_loop(
                bb.as_ref(),
                ss.as_ref(),
                cc.as_ref(),
                rx,
                params(),
                test_pi(),
                Vec::new(),
                std::env::temp_dir().join("pmgfan-test-calib.toml"),
                Duration::from_millis(10),
                3,
                90.0,
                95.0,
                1,
                FailAction::FullSpeed,
            )
            .await
        });
        {
            let mut w = s.write().await;
            w.temps = vec![TempReading {
                chip: "ipmi".into(),
                label: "CPU".into(),
                celsius: 40.0,
            }];
            w.last_temp_ok = Instant::now() - Duration::from_secs(30);
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
        {
            let st = s.read().await;
            assert_eq!(st.pwm, Some(EMERGENCY_PWM));
            assert_eq!(st.state, DaemonState::Degraded);
        }
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
    }

    /// 0 RPM ファンが連続報告されると zero_rpm_detected が立ち
    /// Degraded になる（復帰で解除されることも確認）
    #[tokio::test]
    async fn zero_rpm_fan_sets_failure_domain() {
        let mut mb = MockBackend::new();
        mb.fans = vec![FanReading {
            name: "FAN CPU".into(),
            rpm: Some(0),
            status: FanStatus::Ok,
        }];
        let b = Arc::new(mb);
        let s = Arc::new(RwLock::new(Shared::new(
            Mode::FixedPwm(50),
            vec![],
            PwmScope::All,
        )));
        let (_tx, rx) = watch::channel(false);
        let (bb, ss) = (Arc::clone(&b), Arc::clone(&s));
        let task = tokio::spawn(async move {
            poll_fans_loop(bb.as_ref(), ss.as_ref(), rx, Duration::from_millis(10), 2).await
        });
        tokio::time::sleep(Duration::from_millis(60)).await;
        {
            let st = s.read().await;
            assert!(st.zero_rpm_detected);
            assert_eq!(st.state, DaemonState::Degraded);
            assert!(st.last_error.as_ref().unwrap().contains("FAN CPU"));
        }
        task.abort();
    }

    #[test]
    fn validate_curves_enforces_config_bounds() {
        let p = params();
        let ok = vec![CurveSpec {
            sensor: "cpu".into(),
            points: vec![(35.0, 30.0), (90.0, 100.0)],
        }];
        assert!(validate_curves(&ok, &p).is_ok());
        // 空リストは拒否
        assert!(validate_curves(&[], &p).is_err());
        // 空センサー名は拒否
        assert!(validate_curves(
            &[CurveSpec {
                sensor: " ".into(),
                points: vec![(30.0, 30.0), (90.0, 100.0)]
            }],
            &p
        )
        .is_err());
        // min_pwm 未満は拒否
        assert!(validate_curves(
            &[CurveSpec {
                sensor: "cpu".into(),
                points: vec![(35.0, 20.0), (90.0, 100.0)]
            }],
            &p
        )
        .is_err());
        // 最終点が max_pwm 未満 = 高温域で全開にならない → 拒否
        assert!(validate_curves(
            &[CurveSpec {
                sensor: "cpu".into(),
                points: vec![(35.0, 30.0), (90.0, 80.0)]
            }],
            &p
        )
        .is_err());
        // 温度範囲外は拒否
        assert!(validate_curves(
            &[CurveSpec {
                sensor: "cpu".into(),
                points: vec![(35.0, 30.0), (200.0, 100.0)]
            }],
            &p
        )
        .is_err());
        // 非昇順は Curve::new が拒否
        assert!(validate_curves(
            &[CurveSpec {
                sensor: "cpu".into(),
                points: vec![(90.0, 30.0), (35.0, 100.0)]
            }],
            &p
        )
        .is_err());
    }

    #[test]
    fn persist_curves_rewrites_curve_sections_preserving_rest() {
        let dir = std::env::temp_dir().join(format!("pmgfan-persist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "# comment\n[device]\nmodel = \"X\"\n\n[[curve]]\nsensor = \"old\"\npoints = [[10, 30]]\n",
        )
        .unwrap();
        let curves = vec![
            Curve::new("cpu_package", vec![(35.0, 30.0), (90.0, 100.0)]).unwrap(),
            Curve::new("pch", vec![(45.0, 30.0), (90.0, 100.0)]).unwrap(),
        ];
        persist_curves(&path, &curves).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        // 他セクション・コメントが残る
        assert!(text.contains("# comment"));
        assert!(text.contains("[device]"));
        // 新しいカーブが書き込まれ、古いエントリは消える
        assert!(text.contains("cpu_package"));
        assert!(text.contains("pch"));
        assert!(!text.contains("\"old\""));
        // 書き戻したものが Config として再パースできる
        let cfg: pmgfan_core::config::Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.curves.len(), 2);
        assert_eq!(cfg.curves[0].sensor, "cpu_package");
        assert_eq!(cfg.curves[0].points[1], (90.0, 100.0));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// persist_scope が [control] pwm_scope を上書きし、
    /// [control] が無い設定ではテーブルごと作ること。
    #[test]
    fn persist_scope_updates_control_section() {
        let dir = std::env::temp_dir().join(format!("pmgfan-scope-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        // 既存 [control] の上書き（他キーは保持）
        std::fs::write(
            &path,
            "[control]\nmin_pwm = 10\npwm_scope = \"all\"\n# keep\n",
        )
        .unwrap();
        persist_scope(&path, PwmScope::Chassis).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let cfg: pmgfan_core::config::Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.control.pwm_scope, "chassis");
        assert_eq!(cfg.control.min_pwm, 10);
        assert!(text.contains("# keep"));

        // [control] が無い設定にも追記できる
        std::fs::write(&path, "interval_ms = 500\n").unwrap();
        persist_scope(&path, PwmScope::All).unwrap();
        let cfg: pmgfan_core::config::Config =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cfg.control.pwm_scope, "all");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Target RPM: 参照ファン未指定・範囲外・未知ファン名は拒否、
    /// 既知ファンなら受理される。
    #[tokio::test]
    async fn apply_mode_target_rpm_validation() {
        let b = MockBackend::new();
        let s = shared_with_mode(Mode::IrmcAuto);
        s.write().await.fans = vec![FanReading {
            name: "FAN CPU".into(),
            rpm: Some(2000),
            status: pmgfan_core::fan::FanStatus::Ok,
        }];
        let c = Mutex::new(());
        let p = params();
        // 空ファン名 / 範囲外 RPM
        assert!(apply_mode(
            &b,
            &s,
            &c,
            &p,
            &Mode::TargetRpm {
                fan: " ".into(),
                rpm: 2500
            }
        )
        .await
        .is_err());
        assert!(apply_mode(
            &b,
            &s,
            &c,
            &p,
            &Mode::TargetRpm {
                fan: "FAN CPU".into(),
                rpm: 100
            }
        )
        .await
        .is_err());
        // 一覧が既知なら存在しないファンは拒否
        assert!(apply_mode(
            &b,
            &s,
            &c,
            &p,
            &Mode::TargetRpm {
                fan: "FAN PSU1".into(),
                rpm: 2500
            }
        )
        .await
        .is_err());
        // 正当な値は受理（ファン名の大小・空白は緩和される）
        assert!(apply_mode(
            &b,
            &s,
            &c,
            &p,
            &Mode::TargetRpm {
                fan: " fan cpu ".into(),
                rpm: 2500
            }
        )
        .await
        .is_ok());
        let st = s.read().await;
        assert_eq!(
            st.mode,
            Mode::TargetRpm {
                fan: " fan cpu ".into(),
                rpm: 2500
            }
        );
    }

    /// Target RPM: ファン一覧が未取得（起動直後）でも受理する —
    /// 実際の参照可否は制御ループが毎 tick 判定する。
    #[tokio::test]
    async fn apply_mode_target_rpm_accepts_unknown_fan_before_first_poll() {
        let b = MockBackend::new();
        let s = shared_with_mode(Mode::IrmcAuto);
        let c = Mutex::new(());
        assert!(apply_mode(
            &b,
            &s,
            &c,
            &params(),
            &Mode::TargetRpm {
                fan: "ANY".into(),
                rpm: 2000
            }
        )
        .await
        .is_ok());
    }

    /// Target RPM の制御ループ: 実測が目標を下回ると PWM を上げ、
    /// 参照ファンが消えると解除して Degraded になる。
    #[tokio::test]
    async fn target_rpm_controls_and_failsafe_on_missing_fan() {
        let b = Arc::new(MockBackend::new());
        let s = Arc::new(RwLock::new(Shared::new(
            Mode::TargetRpm {
                fan: "FAN CPU".into(),
                rpm: 2500,
            },
            vec![],
            PwmScope::All,
        )));
        // 制御ループは Shared のファン/温度スナップショットを読む
        // （ポーリングタスクが書き込む側）。テストでは直接注入する。
        {
            let mut w = s.write().await;
            w.fans = vec![FanReading {
                name: "FAN CPU".into(),
                rpm: Some(1800),
                status: pmgfan_core::fan::FanStatus::Ok,
            }];
            w.temps = vec![TempReading {
                chip: "ipmi".into(),
                label: "CPU".into(),
                celsius: 40.0,
            }];
        }
        let c = Arc::new(Mutex::new(()));
        let (tx, rx) = watch::channel(false);
        let (bb, ss, cc) = (Arc::clone(&b), Arc::clone(&s), Arc::clone(&c));
        let task = tokio::spawn(async move {
            control_loop(
                bb.as_ref(),
                ss.as_ref(),
                cc.as_ref(),
                rx,
                params(),
                test_pi(),
                Vec::new(),
                std::env::temp_dir().join("pmgfan-test-calib.toml"),
                Duration::from_millis(10),
                3,
                90.0,
                95.0,
                60,
                FailAction::IrmcAuto,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(150)).await;
        {
            let st = s.read().await;
            // 実測 1800 < 目標 2500 → min より上の PWM が書かれる
            assert!(st.pwm.unwrap_or(0) > 30);
            assert_eq!(st.state, DaemonState::Controlling);
            assert!(!st.target_fan_missing);
        }
        // 参照ファンが消えた → 解除して Degraded
        s.write().await.fans.clear();
        tokio::time::sleep(Duration::from_millis(100)).await;
        {
            let st = s.read().await;
            assert!(st.target_fan_missing);
            assert_eq!(st.pwm, None);
            assert_eq!(st.state, DaemonState::Degraded);
        }
        assert!(b.clear_calls.load(Ordering::SeqCst) >= 1);
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
    }

    /// CalibJob: min..=100 を 10 刻みで掃引し、最後に Done で
    /// 中央値を返す。tick 時刻を注入して時間を圧縮する。
    #[test]
    fn calib_job_sweeps_levels_and_returns_medians() {
        let mut job = CalibJob::new(&params()); // min_pwm=30 → 30..=100 step 10 = 8 levels +100 cap? (30,40,...,100) = 8
        assert_eq!(job.levels.len(), 8);
        let fans = || {
            vec![FanReading {
                name: "FAN CPU".into(),
                rpm: Some(2000),
                status: pmgfan_core::fan::FanStatus::Ok,
            }]
        };
        let mut now = Instant::now();
        let mut done: Option<Vec<CalibPoint>> = None;
        for _ in 0..10000 {
            match job.tick(&fans(), now) {
                CalibStep::Write(_) => {}
                CalibStep::Wait => {}
                CalibStep::Done(pts) => {
                    done = Some(pts);
                    break;
                }
            }
            now += Duration::from_secs(5); // settle(8s) 2tick + sample(4s) 1tick で1レベル
        }
        let pts = done.expect("job must complete");
        assert_eq!(pts.len(), 8);
        assert_eq!(pts[0].pwm, 30);
        assert_eq!(pts[7].pwm, 100);
        assert_eq!(pts[0].rpm, vec![("FAN CPU".to_string(), 2000)]);
    }

    /// キャリブレーション結果の保存→読込の往復。壊れたファイルは
    /// 空として扱う。
    #[test]
    fn calibration_persist_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("pmgfan-calib-{}", std::process::id()));
        let path = dir.join("calibration.toml");
        let points = vec![
            CalibPoint {
                pwm: 30,
                rpm: vec![("FAN CPU".into(), 1450), ("FAN1 SYS".into(), 1600)],
            },
            CalibPoint {
                pwm: 100,
                rpm: vec![("FAN CPU".into(), 5200), ("FAN1 SYS".into(), 5400)],
            },
        ];
        persist_calibration(&path, &points).unwrap();
        let loaded = load_calibration(&path);
        assert_eq!(loaded, points);

        // 壊れたファイル → 空
        std::fs::write(&path, "not [toml").unwrap();
        assert!(load_calibration(&path).is_empty());
        // 存在しない → 空
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(load_calibration(&path).is_empty());
    }

    /// estimate_pwm: キャリブレーション表からの線形補間と
    /// 範囲外クランプ。
    #[test]
    fn estimate_pwm_interpolates() {
        let table = vec![
            CalibPoint {
                pwm: 30,
                rpm: vec![("FAN CPU".into(), 1500)],
            },
            CalibPoint {
                pwm: 50,
                rpm: vec![("FAN CPU".into(), 2500)],
            },
        ];
        assert_eq!(estimate_pwm(&table, "FAN CPU", 2000), Some(40.0));
        // 範囲外は端点
        assert_eq!(estimate_pwm(&table, "FAN CPU", 100), Some(30.0));
        assert_eq!(estimate_pwm(&table, "FAN CPU", 9999), Some(50.0));
        // そのファンの列が無ければ None
        assert_eq!(estimate_pwm(&table, "FAN PSU1", 2000), None);
    }

    /// apply_mode で任意のモードへ切替すると実行中の
    /// キャリブレーション状態がクリアされる（中断経路）。
    #[tokio::test]
    async fn apply_mode_aborts_calibration() {
        let b = MockBackend::new();
        let s = shared_with_mode(Mode::Calibrate);
        s.write().await.calibration = Some(CalibStatus {
            active: true,
            current_pwm: Some(50),
            step: 2,
            total: 8,
            points: vec![],
            result: None,
        });
        let c = Mutex::new(());
        apply_mode(&b, &s, &c, &params(), &Mode::IrmcAuto)
            .await
            .unwrap();
        assert!(s.read().await.calibration.is_none());
    }

    #[test]
    fn curve_demand_reports_missing() {
        let c1 = Curve::new("cpu_package", vec![(30.0, 30.0), (80.0, 80.0)]).unwrap();
        let c2 = Curve::new("nonexistent", vec![(30.0, 30.0), (80.0, 80.0)]).unwrap();
        let temps = vec![TempReading {
            chip: "ipmi".into(),
            label: "CPU".into(),
            celsius: 40.0,
        }];
        let (demand, missing) = curve_demand(&[c1, c2], &temps);
        assert!(demand.is_some()); // 解決できたカーブだけで最大値
        assert_eq!(missing, vec!["nonexistent".to_string()]);
        // 全滅
        let (demand, _) = curve_demand(
            &[Curve::new("zzz", vec![(30.0, 30.0), (80.0, 80.0)]).unwrap()],
            &temps,
        );
        assert_eq!(demand, None);
    }
}
