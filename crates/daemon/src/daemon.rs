//! pmgfand デーモン本体。
//! Phase 2: 監視 + Unix socket。Phase 3/4: 制御ループ（Fixed PWM / iRMC Auto / Curve）。

use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use pmgfan_core::control::{ControlParams, RateLimiter};
use pmgfan_core::curve::Curve;
use pmgfan_core::fan::FanReading;
use pmgfan_core::hwmon;
use pmgfan_core::protocol::{DaemonState, Mode};
use pmgfan_core::sensor::{self, TempReading};
use pmgfan_ipmi::backend::FanControlBackend;
use pmgfan_ipmi::fujitsu;
use tokio::net::UnixDatagram;
use tokio::sync::{Mutex, RwLock};
use tokio::time::interval;
use tracing::{error, info, warn};

use crate::ipc;

/// PWM 値が変わらなくても強制値を再送する間隔（適用 tick 数）。
const REASSERT_TICKS: u32 = 6;

/// デーモン動作パラメータ（config または既定値から構築）。
#[derive(Debug, Clone)]
pub struct Params {
    pub socket_path: PathBuf,
    pub poll_interval: Duration,
    pub apply_interval: Duration,
    pub control: ControlParams,
    pub curves: Vec<Curve>,
    pub startup_mode: Mode,
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
    pub ipmi_failures: u32,
    pub started: Instant,
}

impl Shared {
    fn new(startup_mode: Mode) -> Self {
        Self {
            state: DaemonState::Starting,
            mode: startup_mode,
            mode_generation: 0,
            pwm: None,
            fans: Vec::new(),
            temps: Vec::new(),
            last_error: None,
            ipmi_failures: 0,
            started: Instant::now(),
        }
    }
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
    let shared = Arc::new(RwLock::new(Shared::new(params.startup_mode.clone())));
    let socket_path = params.socket_path.clone();

    // 単一インスタンスロック。2重起動は IPMI 操作・socket 掃除で
    // 互いに干渉するため、起動直後に fail-fast で防ぐ。
    let run_dir = socket_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("/run/pmgfand"));
    let _instance_lock = acquire_instance_lock(&run_dir)?;

    // センサーポーリング
    let poll = {
        let backend = Arc::clone(&backend);
        let shared = Arc::clone(&shared);
        let poll_interval = params.poll_interval;
        tokio::spawn(async move { poll_loop(&*backend, &shared, poll_interval).await })
    };

    // PWM/override 操作の直列化ロック。制御ループの書き込みと
    // apply_mode の即時解除が入れ違いで「iRMC Auto 表示のまま
    // override が残る」ことを防ぐ。
    let ctrl = Arc::new(Mutex::new(()));

    // 制御ループ（PWM 書き込みの単一主体）
    let control = {
        let backend = Arc::clone(&backend);
        let shared = Arc::clone(&shared);
        let ctrl = Arc::clone(&ctrl);
        let apply_interval = params.apply_interval;
        let control_params = params.control;
        let curves = params.curves.clone();
        tokio::spawn(async move {
            control_loop(&*backend, &shared, &ctrl, control_params, &curves, apply_interval)
                .await
        })
    };

    // Unix socket
    let ipc_task = {
        let backend = Arc::clone(&backend);
        let shared = Arc::clone(&shared);
        let curves = Arc::new(params.curves.clone());
        let path = socket_path.clone();
        tokio::spawn(async move { ipc::serve(&path, backend, shared, curves, ctrl).await })
    };

    // systemd watchdog（WATCHDOG_USEC があれば半周期でキック）
    spawn_watchdog();

    // 起動モードを反映
    {
        let mut s = shared.write().await;
        s.state = match s.mode {
            Mode::IrmcAuto => DaemonState::Monitoring,
            _ => DaemonState::Controlling,
        };
        if s.mode == Mode::IrmcAuto {
            // 前回の強制が残っている可能性を潰す
            if let Err(e) = backend.clear_override().await {
                warn!(error = %e, "startup clear_override failed");
            }
        }
    }
    sd_notify("READY=1").await;
    info!(socket = %socket_path.display(), mode = ?params.startup_mode, "pmgfand started");

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => info!("SIGINT received"),
        _ = sigterm.recv() => info!("SIGTERM received"),
        r = poll => error!(?r, "poller exited"),
        r = control => error!(?r, "control loop exited"),
        r = ipc_task => error!(?r, "ipc server exited"),
    }

    // シャットダウン: 必ず iRMC 自動制御へ戻す
    info!("shutting down; clearing OEM override");
    if let Err(e) = backend.clear_override().await {
        error!(error = %e, "failed to clear override on shutdown");
    }
    sd_notify("STOPPING=1").await;
    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

async fn poll_loop<B: FanControlBackend>(
    backend: &B,
    shared: &RwLock<Shared>,
    poll_interval: Duration,
) {
    let mut tick = interval(poll_interval);
    loop {
        tick.tick().await;
        let mut s = shared.write().await;
        match backend.fans().await {
            Ok(fans) => {
                s.fans = fans;
                s.ipmi_failures = 0;
                s.last_error = None;
            }
            Err(e) => {
                s.ipmi_failures += 1;
                s.last_error = Some(e.to_string());
                if s.ipmi_failures >= 3 {
                    s.state = DaemonState::Degraded;
                    warn!(failures = s.ipmi_failures, error = %e, "ipmi polling failing");
                }
            }
        }
        match backend.temperatures().await {
            Ok(mut temps) => {
                temps.extend(read_hwmon());
                s.temps = temps;
            }
            Err(e) => {
                // IPMI 温度が取れなくても hwmon だけは更新する
                s.temps = read_hwmon();
                warn!(error = %e, "ipmi temperature read failed");
            }
        }
    }
}

/// 制御ループ。PWM への書き込みはここだけが行う
/// （`apply_mode` の IrmcAuto 即時解除を除く）。
async fn control_loop<B: FanControlBackend>(
    backend: &B,
    shared: &RwLock<Shared>,
    ctrl: &Mutex<()>,
    params: ControlParams,
    curves: &[Curve],
    apply_interval: Duration,
) {
    let mut limiter = RateLimiter::new(params);
    let mut seen_gen = 0u64;
    let mut ticks_since_write = 0u32;
    let mut tick = interval(apply_interval);

    loop {
        tick.tick().await;
        // PWM 操作は apply_mode の即時解除と直列化する
        let _ctrl = ctrl.lock().await;
        let (mode, gen, temps) = {
            let s = shared.read().await;
            (s.mode.clone(), s.mode_generation, s.temps.clone())
        };
        if gen != seen_gen {
            limiter.reset();
            ticks_since_write = 0;
            seen_gen = gen;
        }

        match mode {
            Mode::IrmcAuto => {}
            Mode::FixedPwm(p) => {
                // 外部要因で解除される可能性に備え周期再アサート。
                // ただし値が変わった tick で即時書き込み、変わらなければ
                // REASSERT_TICKS ごとに再送する。
                let cur = { shared.read().await.pwm };
                ticks_since_write += 1;
                if cur != Some(p) || ticks_since_write >= REASSERT_TICKS {
                    write_pwm(backend, shared, p).await;
                    ticks_since_write = 0;
                }
            }
            Mode::Curve => {
                let target = curve_demand(curves, &temps);
                match target {
                    None => {
                        // 参照センサーが全滅 → 独自制御は捨てて iRMC へ戻す
                        let needs_clear = shared.read().await.pwm.is_some();
                        if needs_clear {
                            warn!("no curve sensors readable; clearing override (iRMC auto)");
                            if let Err(e) = backend.clear_override().await {
                                error!(error = %e, "clear_override failed");
                            }
                        }
                        let mut s = shared.write().await;
                        if needs_clear {
                            s.pwm = None;
                        }
                        s.state = DaemonState::Degraded;
                    }
                    Some(target) => match limiter.next(target) {
                        Some(p) => {
                            write_pwm(backend, shared, p).await;
                            ticks_since_write = 0;
                        }
                        None => {
                            ticks_since_write += 1;
                            if ticks_since_write >= REASSERT_TICKS {
                                if let Some(p) = limiter.current() {
                                    write_pwm(backend, shared, p).await;
                                }
                                ticks_since_write = 0;
                            }
                        }
                    },
                }
            }
            Mode::TargetRpm { .. } => {
                // Phase 7 で実装。set_mode では拒否済みだが
                // config 直書き等で来た場合の保険。
                warn!("target_rpm mode is not implemented yet");
            }
        }
    }
}

/// 全カーブを現在温度で評価し、最大要求 PWM を返す。
/// 1つも解決できなければ `None`。
fn curve_demand(curves: &[Curve], temps: &[TempReading]) -> Option<u8> {
    curves
        .iter()
        .filter_map(|c| {
            let t = sensor::resolve(temps, &c.sensor);
            if t.is_none() {
                warn!(sensor = %c.sensor, "curve sensor not readable");
            }
            t.map(|t| c.eval(t.celsius as f32).round().clamp(0.0, 100.0) as u8)
        })
        .max()
}

async fn write_pwm<B: FanControlBackend>(backend: &B, shared: &RwLock<Shared>, pwm: u8) {
    match backend.set_global_pwm(pwm).await {
        Ok(()) => {
            let mut s = shared.write().await;
            s.pwm = Some(pwm);
            s.state = DaemonState::Controlling;
            s.ipmi_failures = 0;
            s.last_error = None;
        }
        Err(e) => {
            let mut s = shared.write().await;
            s.ipmi_failures += 1;
            s.last_error = Some(e.to_string());
            if s.ipmi_failures >= 3 {
                s.state = DaemonState::Degraded;
            }
            warn!(error = %e, "pwm write failed");
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
    std::fs::create_dir_all(run_dir).with_context(|| {
        format!("cannot create runtime directory {}", run_dir.display())
    })?;
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
    curves: &[Curve],
    mode: &Mode,
) -> std::result::Result<(), String> {
    match mode {
        Mode::IrmcAuto => {
            // 制御ループの書き込みと直列化。ここが先なら次 tick で
            // Auto を見て書き込みを止め、後ならこの解除が最終状態になる。
            let _ctrl = ctrl.lock().await;
            backend.clear_override().await.map_err(|e| e.to_string())?;
            let mut s = shared.write().await;
            s.mode = Mode::IrmcAuto;
            s.mode_generation += 1;
            s.pwm = None;
            s.state = DaemonState::Monitoring;
            info!("mode -> irmc_auto");
            Ok(())
        }
        Mode::FixedPwm(p) => {
            if *p > fujitsu::MAX_PWM {
                return Err(format!("pwm must be 0..=100, got {p}"));
            }
            if *p < fujitsu::MIN_SAFE_PWM {
                return Err(format!(
                    "pwm {p}% is below the safety floor {}%",
                    fujitsu::MIN_SAFE_PWM
                ));
            }
            let mut s = shared.write().await;
            s.mode = mode.clone();
            s.mode_generation += 1;
            info!(pwm = p, "mode -> fixed_pwm");
            Ok(())
        }
        Mode::Curve => {
            if curves.is_empty() {
                return Err("no fan curves configured".into());
            }
            let mut s = shared.write().await;
            s.mode = Mode::Curve;
            s.mode_generation += 1;
            info!("mode -> curve");
            Ok(())
        }
        Mode::TargetRpm { .. } => {
            Err("mode not implemented yet (roadmap phase 7)".into())
        }
    }
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
fn spawn_watchdog() {
    let Ok(usec) = std::env::var("WATCHDOG_USEC") else {
        return;
    };
    let Ok(usec) = usec.parse::<u64>() else {
        return;
    };
    if usec == 0 {
        return;
    }
    let half = Duration::from_micros(usec) / 2;
    tokio::spawn(async move {
        let mut t = interval(half);
        loop {
            t.tick().await;
            sd_notify("WATCHDOG=1").await;
        }
    });
    info!(watchdog_usec = usec, "systemd watchdog enabled");
}
