//! pmgfand デーモン本体（Phase 2: 監視 + Unix socket + 基本的なモード適用）。

use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use pmgfan_core::fan::FanReading;
use pmgfan_core::hwmon;
use pmgfan_core::protocol::{DaemonState, Mode};
use pmgfan_core::sensor::TempReading;
use pmgfan_ipmi::backend::FanControlBackend;
use pmgfan_ipmi::fujitsu;
use tokio::net::UnixDatagram;
use tokio::sync::RwLock;
use tokio::time::interval;
use tracing::{error, info, warn};

use crate::ipc;

/// ポーリング周期（Phase 2 では固定。config 対応は Phase 4）。
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// デーモンの共有状態。
#[derive(Debug)]
pub struct Shared {
    pub state: DaemonState,
    pub mode: Mode,
    /// 現在強制している PWM。モードが強制系でない場合 None。
    pub pwm: Option<u8>,
    pub fans: Vec<FanReading>,
    pub temps: Vec<TempReading>,
    /// 直近の IPMI エラー（診断用）
    pub last_error: Option<String>,
    pub ipmi_failures: u32,
    pub started: Instant,
}

impl Shared {
    fn new() -> Self {
        Self {
            state: DaemonState::Starting,
            mode: Mode::IrmcAuto,
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
pub async fn run<B>(backend: B, socket_path: PathBuf) -> Result<()>
where
    B: FanControlBackend + Send + Sync + 'static,
{
    let backend = Arc::new(backend);
    let shared = Arc::new(RwLock::new(Shared::new()));

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
        tokio::spawn(async move { poll_loop(&*backend, &shared).await })
    };

    // Unix socket
    let ipc = {
        let backend = Arc::clone(&backend);
        let shared = Arc::clone(&shared);
        let path = socket_path.clone();
        tokio::spawn(async move { ipc::serve(&path, backend, shared).await })
    };

    // systemd watchdog（WATCHDOG_USEC があれば半周期でキック）
    spawn_watchdog();

    {
        let mut s = shared.write().await;
        s.state = DaemonState::Monitoring;
    }
    sd_notify("READY=1").await;
    info!(socket = %socket_path.display(), "pmgfand started");

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => info!("SIGINT received"),
        _ = sigterm.recv() => info!("SIGTERM received"),
        r = poll => error!(?r, "poller exited"),
        r = ipc => error!(?r, "ipc server exited"),
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

async fn poll_loop<B: FanControlBackend>(backend: &B, shared: &RwLock<Shared>) {
    let mut tick = interval(POLL_INTERVAL);
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

/// モード変更を適用する。IPC ハンドラから呼ばれる。
///
/// Phase 2 では `IrmcAuto` / `FixedPwm` のみ実装。
/// `Curve` / `TargetRpm` は後続フェーズ。
pub async fn apply_mode<B: FanControlBackend>(
    backend: &B,
    shared: &RwLock<Shared>,
    mode: &Mode,
) -> std::result::Result<(), String> {
    match mode {
        Mode::IrmcAuto => {
            backend.clear_override().await.map_err(|e| e.to_string())?;
            let mut s = shared.write().await;
            s.mode = Mode::IrmcAuto;
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
            backend.set_global_pwm(*p).await.map_err(|e| e.to_string())?;
            let mut s = shared.write().await;
            s.mode = mode.clone();
            s.pwm = Some(*p);
            s.state = DaemonState::Controlling;
            info!(pwm = p, "mode -> fixed_pwm");
            Ok(())
        }
        Mode::Curve | Mode::TargetRpm { .. } => {
            Err("mode not implemented yet (roadmap phase 4/7)".into())
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
