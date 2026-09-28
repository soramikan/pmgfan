//! pmgfand — PRIMERGY TX1320 M4 ファン制御デーモン。
//!
//! サブコマンドなしで起動するとデーモンとして動作する
//! （systemd の ExecStart 想定。socket + 監視ループ）。
//! その他のサブコマンドは単発操作・診断用。

mod daemon;
mod ipc;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use pmgfan_core::config::Config;
use pmgfan_core::control::{ControlParams, PiParams, PwmScope};
use pmgfan_core::curve::Curve;
use pmgfan_core::fan::FanReading;
use pmgfan_core::hwmon;
use pmgfan_core::protocol::{self, Mode};
use pmgfan_core::sensor::TempReading;
use pmgfan_ipmi::backend::FanControlBackend;
use pmgfan_ipmi::fujitsu;
use pmgfan_ipmi::ipmitool::IpmitoolBackend;
use tokio::time::sleep;

const EXPECTED_PRODUCT: &str = "PRIMERGY TX1320 M4";

#[derive(Parser)]
#[command(
    name = "pmgfand",
    version,
    about = "PRIMERGY TX1320 M4 iRMC fan control daemon"
)]
struct Cli {
    /// ipmitool バイナリ
    #[arg(long, global = true, default_value = "ipmitool")]
    ipmitool: OsString,
    /// ipmitool -I のインターフェース（未指定時は config [device] interface → "open"）
    #[arg(short = 'I', long, global = true)]
    interface: Option<String>,
    /// 設定ファイル（省略時は既定値）
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// 制御 socket のパス
    #[arg(long, global = true, default_value_os_t = PathBuf::from(protocol::DEFAULT_SOCKET_PATH))]
    socket: PathBuf,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// デーモンとして起動（省略時のデフォルト）
    Run,
    /// 機種・iRMC の疎通と製品名の検証
    Probe,
    /// ファン SDR を1回表示
    Fans,
    /// 温度（hwmon + IPMI SDR）を1回表示
    Temps,
    /// ファン・温度を定期表示
    Monitor {
        /// ポーリング間隔（秒）
        #[arg(short = 'n', long, default_value_t = 2.0)]
        interval: f64,
    },
    /// PWM を % 固定する（既定は全ファン。--chassis で PSU を除く）
    SetPwm {
        /// PWM duty (%)
        percent: u8,
        /// 30% 未満を許可する
        #[arg(long)]
        allow_low: bool,
        /// シャーシファン（FAN CPU / FANx SYS）のみに適用し、
        /// PSU ファンは iRMC 自動制御に残す
        #[arg(long)]
        chassis: bool,
    },
    /// PWM force スロットを読み出す
    Read {
        /// スロット index（10進 or 0x..）。省略時は 0 1 0x19 0x1a
        indices: Vec<String>,
    },
    /// PWM 強制を解除して iRMC 自動制御へ戻す
    ClearOverride,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let bin = cli.ipmitool.clone();
    let cli_iface = cli.interface.clone();

    match cli.cmd.unwrap_or(Cmd::Run) {
        Cmd::Run => {
            let (params, cfg_iface) = build_params(cli.config.as_deref(), cli.socket.clone())?;
            let iface = cli_iface.unwrap_or(cfg_iface);
            let backend = IpmitoolBackend::new(bin, iface);
            daemon::run(backend, params).await
        }
        cmd => {
            // 単発コマンドでも --config の [device].interface を使う
            // （-I 指定が最優先）
            let cfg_iface = load_config_interface(cli.config.as_deref())?;
            let backend = IpmitoolBackend::new(
                bin,
                cli_iface.or(cfg_iface).unwrap_or_else(|| "open".into()),
            );
            match cmd {
                Cmd::Probe => probe(&backend).await,
                Cmd::Fans => {
                    print_fans(&backend.fans().await?);
                    Ok(())
                }
                Cmd::Temps => {
                    let mut temps = backend.temperatures().await?;
                    temps.extend(
                        hwmon::read_temperatures(Path::new(hwmon::HWMON_ROOT)).unwrap_or_default(),
                    );
                    print_temps(&temps);
                    Ok(())
                }
                Cmd::Monitor { interval } => monitor(&backend, interval).await,
                Cmd::SetPwm {
                    percent,
                    allow_low,
                    chassis,
                } => {
                    if percent > fujitsu::MAX_PWM {
                        bail!("percent must be 0..=100, got {percent}");
                    }
                    if percent < fujitsu::MIN_SAFE_PWM && !allow_low {
                        bail!(
                            "refusing to set below {}%; pass --allow-low if you really want that",
                            fujitsu::MIN_SAFE_PWM
                        );
                    }
                    let scope = if chassis {
                        PwmScope::Chassis
                    } else {
                        PwmScope::All
                    };
                    backend.set_pwm(scope, percent).await?;
                    println!(
                        "set {} PWM channels to {percent}%",
                        if chassis { "chassis" } else { "all" }
                    );
                    print_fans(&backend.fans().await?);
                    Ok(())
                }
                Cmd::Read { indices } => {
                    let indices = parse_indices(&indices)?;
                    let slots = backend.read_override_slots(&indices).await?;
                    if slots.is_empty() {
                        println!("(empty response)");
                    }
                    for s in slots {
                        println!(
                            "slot 0x{:02x} = 0x{:02x} ({}){}",
                            s.index,
                            s.value,
                            s.value,
                            if s.forced { " FORCED" } else { "" }
                        );
                    }
                    Ok(())
                }
                Cmd::ClearOverride => {
                    backend.clear_override().await?;
                    println!("cleared PWM override; iRMC automatic control restored");
                    print_fans(&backend.fans().await?);
                    Ok(())
                }
                Cmd::Run => unreachable!("handled above"),
            }
        }
    }
}

/// `--config` が指定されている場合だけ設定を読み、
/// `[device].interface` を返す（単発コマンド向けの軽量読み込み。
/// デーモン用の全検証は `build_params` が行う）。
fn load_config_interface(config_path: Option<&Path>) -> Result<Option<String>> {
    let Some(path) = config_path else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read config {}", path.display()))?;
    let config: Config =
        toml::from_str(&text).with_context(|| format!("invalid TOML in {}", path.display()))?;
    Ok(Some(config.device.interface))
}

/// `--config`（あれば読み込み、なければ既定値）から
/// `daemon::Params` と ipmitool インターフェース名を構築する。
///
/// 設定値はここで検証する。IPC 側の検査（apply_mode）を
/// 迂回しないよう、不整合な設定は起動時エラーにする。
fn build_params(config_path: Option<&Path>, socket: PathBuf) -> Result<(daemon::Params, String)> {
    let config: Config = match config_path {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("cannot read config {}", path.display()))?;
            toml::from_str(&text)
                .with_context(|| format!("cannot parse config {}", path.display()))?
        }
        None => Config::default(),
    };

    let curves: Vec<Curve> = config
        .curves
        .iter()
        .map(|c| Curve::new(c.sensor.clone(), c.points.clone()))
        .collect::<std::result::Result<_, _>>()
        .context("invalid curve in config")?;

    let cc = &config.control;
    // PWM 範囲・ステップの不変条件。min_pwm の既定は 30% だが
    // 実機ではそれ以下でもファンは回転継続するため、config で
    // 明示した値はそのまま下限として使う（下限を下げると
    // 冷却余力が減るので警告は出す）
    if cc.min_pwm > cc.max_pwm || cc.max_pwm > fujitsu::MAX_PWM {
        bail!(
            "[control] invalid pwm range min={} max={} (need 0..={})",
            cc.min_pwm,
            cc.max_pwm,
            fujitsu::MAX_PWM
        );
    }
    if cc.min_pwm < fujitsu::MIN_SAFE_PWM {
        eprintln!(
            "warning: [control] min_pwm {}% is below the default safety floor {}%",
            cc.min_pwm,
            fujitsu::MIN_SAFE_PWM
        );
    }
    // 強制 PWM の適用範囲。"chassis" で PSU ファンを iRMC 自動制御に残す
    let pwm_scope = PwmScope::parse(&cc.pwm_scope).map_err(anyhow::Error::msg)?;
    if cc.step_up == 0 || cc.step_down == 0 {
        bail!("[control] step_up/step_down must be >= 1");
    }
    if config.safety.ipmi_failure_limit == 0 {
        bail!("[safety] ipmi_failure_limit must be >= 1");
    }
    if config.safety.sensor_stale_seconds == 0 {
        bail!("[safety] sensor_stale_seconds must be >= 1");
    }
    let fail_action =
        daemon::FailAction::parse(&config.safety.fail_action).map_err(anyhow::Error::msg)?;
    // 機種照合は空文字だと contains() が常に真となり
    // 検証が無効化されるため拒否する
    if config.device.model.trim().is_empty() {
        bail!("[device] model must not be empty");
    }
    // 空 interface だと `ipmitool -I ""` になる
    if config.device.interface.trim().is_empty() {
        bail!("[device] interface must not be empty");
    }
    // 現状 ipmitool バックエンドのみ実装（native は Phase 9）。
    // 未対応値を黙って無視しない
    if config.device.backend != "ipmitool" {
        bail!(
            "[device] backend '{}' is not supported yet (only \"ipmitool\" is implemented)",
            config.device.backend
        );
    }
    for (key, v) in [
        ("cpu_emergency", config.safety.cpu_emergency),
        ("pch_emergency", config.safety.pch_emergency),
    ] {
        if !v.is_finite() || !(0.0..=150.0).contains(&v) {
            bail!("[safety] {key} must be a finite temperature in 0..=150 (got {v})");
        }
    }

    let startup_mode = match cc.mode.as_str() {
        "auto" | "irmc_auto" => Mode::IrmcAuto,
        "fixed_pwm" => {
            let p = cc
                .fixed_pwm
                .context("[control] fixed_pwm is required when mode = \"fixed_pwm\"")?;
            if !(cc.min_pwm..=cc.max_pwm).contains(&p) {
                bail!(
                    "[control] fixed_pwm {p} is outside configured range {}..={}",
                    cc.min_pwm,
                    cc.max_pwm
                );
            }
            Mode::FixedPwm(p)
        }
        "curve" => {
            if curves.is_empty() {
                bail!("mode = \"curve\" requires at least one [[curve]] section");
            }
            Mode::Curve
        }
        "target_rpm" => {
            let tr = config
                .target_rpm
                .as_ref()
                .context("mode = \"target_rpm\" requires a [target_rpm] section")?;
            if tr.reference_fan.trim().is_empty() {
                bail!("[target_rpm] reference_fan must not be empty");
            }
            if !(500..=20000).contains(&tr.target) {
                bail!(
                    "[target_rpm] target {} out of sane range 500..=20000",
                    tr.target
                );
            }
            Mode::TargetRpm {
                fan: tr.reference_fan.clone(),
                rpm: tr.target,
            }
        }
        other => bail!("unknown [control] mode '{other}'"),
    };

    // PI パラメータ。[target_rpm] が無くても既定値で構築する
    // （TUI/CLI からの set_mode でも同じ係数を使う）。
    let tr = config.target_rpm.unwrap_or_default();
    let pi = {
        let min = tr.min_pwm.unwrap_or(cc.min_pwm);
        let max = tr.max_pwm.unwrap_or(cc.max_pwm);
        if min > max || max > fujitsu::MAX_PWM {
            bail!("[target_rpm] invalid pwm range min={min} max={max}");
        }
        if !tr.kp.is_finite() || tr.kp < 0.0 || !tr.ki.is_finite() || tr.ki < 0.0 {
            bail!("[target_rpm] kp/ki must be finite and >= 0");
        }
        if !tr.deadband.is_finite() || tr.deadband < 0.0 {
            bail!("[target_rpm] deadband must be finite and >= 0");
        }
        PiParams {
            kp: tr.kp,
            ki: tr.ki,
            deadband_rpm: tr.deadband,
            min_pwm: min,
            max_pwm: max,
        }
    };
    // キャリブレーション表（無ければ空。PI の初期 PWM 推定に使う）
    let calibration = daemon::load_calibration(Path::new(protocol::DEFAULT_CALIBRATION_PATH));

    let params = daemon::Params {
        socket_path: socket,
        fan_interval: Duration::from_millis(config.monitor.fan_interval_ms.max(200)),
        temp_interval: Duration::from_millis(config.monitor.temperature_interval_ms.max(200)),
        apply_interval: cc.apply_interval(),
        control: ControlParams {
            min_pwm: cc.min_pwm,
            max_pwm: cc.max_pwm,
            pwm_scope,
            step_up: cc.step_up,
            step_down: cc.step_down,
            down_hysteresis: cc.down_hysteresis,
        },
        pi,
        calibration,
        calibration_path: PathBuf::from(protocol::DEFAULT_CALIBRATION_PATH),
        curves,
        startup_mode,
        ipmi_failure_limit: config.safety.ipmi_failure_limit.max(1),
        cpu_emergency: config.safety.cpu_emergency,
        pch_emergency: config.safety.pch_emergency,
        sensor_stale: Duration::from_secs(config.safety.sensor_stale_seconds),
        fail_action,
        expected_model: config.device.model.clone(),
        config_path: config_path.map(|p| p.to_path_buf()),
    };
    Ok((params, config.device.interface))
}

async fn probe(backend: &IpmitoolBackend) -> Result<()> {
    let mc = backend.mc_info().await.context("ipmitool mc info failed")?;
    for line in mc.lines() {
        let l = line.trim();
        if l.starts_with("Manufacturer")
            || l.starts_with("Product")
            || l.starts_with("Firmware")
            || l.starts_with("IPMI Version")
        {
            println!("{l}");
        }
    }
    let fru = backend.fru().await.context("ipmitool fru print failed")?;
    let product = fru.lines().find_map(|l| {
        l.split_once(':')
            .filter(|(k, _)| k.trim() == "Product Name")
            .map(|(_, v)| v.trim().to_string())
    });
    match product {
        Some(p) if p.contains(EXPECTED_PRODUCT) => println!("Product Name : {p}  [OK]"),
        Some(p) => println!("Product Name : {p}  [WARN: expected '{EXPECTED_PRODUCT}']"),
        None => println!("Product Name : (not found)  [WARN]"),
    }
    Ok(())
}

async fn monitor(backend: &IpmitoolBackend, interval: f64) -> Result<()> {
    let start = Instant::now();
    loop {
        println!("== t+{:.1}s ==============", start.elapsed().as_secs_f64());
        match backend.fans().await {
            Ok(fans) => print_fans(&fans),
            Err(e) => println!("  fans: {e}"),
        }
        let mut temps = match backend.temperatures().await {
            Ok(t) => t,
            Err(e) => {
                println!("  ipmi temps: {e}");
                Vec::new()
            }
        };
        temps.extend(hwmon::read_temperatures(Path::new(hwmon::HWMON_ROOT)).unwrap_or_default());
        print_temps(&temps);
        sleep(Duration::from_secs_f64(interval.max(0.2))).await;
    }
}

fn print_fans(fans: &[FanReading]) {
    for f in fans {
        match f.rpm {
            Some(rpm) => println!("  {:<12} {rpm:>5} RPM   {}", f.name, f.status.as_str()),
            None => println!("  {:<12} {:>5}       {}", f.name, "--", f.status.as_str()),
        }
    }
}

fn print_temps(temps: &[TempReading]) {
    for t in temps {
        println!("  {:<16} {:<20} {:>5.1} C", t.chip, t.label, t.celsius);
    }
}

fn parse_indices(args: &[String]) -> Result<Vec<u8>> {
    if args.is_empty() {
        return Ok(fujitsu::DEFAULT_SLOT_INDICES.to_vec());
    }
    if args.len() > 31 {
        bail!("too many slot indices (max 31)");
    }
    args.iter()
        .map(|s| {
            let s = s.trim();
            let idx = if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                u8::from_str_radix(hex, 16).map_err(|e| anyhow::anyhow!("bad index '{s}': {e}"))?
            } else {
                s.parse::<u8>()
                    .map_err(|e| anyhow::anyhow!("bad index '{s}': {e}"))?
            };
            if idx > 31 {
                bail!("slot index out of range 0..31: {idx}");
            }
            Ok(idx)
        })
        .collect()
}
