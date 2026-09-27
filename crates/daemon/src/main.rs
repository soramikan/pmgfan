//! pmgfand — PRIMERGY TX1320 M4 ファン制御デーモン（Phase 1: 監視 + OEM PWM set/clear）

use std::ffi::OsString;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use pmgfan_core::fan::FanReading;
use pmgfan_core::hwmon;
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
    about = "PRIMERGY TX1320 M4 iRMC fan control (Phase 1: monitor + OEM PWM set/clear)"
)]
struct Cli {
    /// ipmitool バイナリ
    #[arg(long, global = true, default_value = "ipmitool")]
    ipmitool: OsString,
    /// ipmitool -I のインターフェース
    #[arg(short = 'I', long, global = true, default_value = "open")]
    interface: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
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
    /// 全 PWM チャンネルを % 固定する
    SetPwm {
        /// PWM duty (%)
        percent: u8,
        /// 30% 未満を許可する
        #[arg(long)]
        allow_low: bool,
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
    let backend = IpmitoolBackend::new(&cli.ipmitool, &cli.interface);

    match cli.cmd {
        Cmd::Probe => probe(&backend).await,
        Cmd::Fans => {
            print_fans(&backend.fans().await?);
            Ok(())
        }
        Cmd::Temps => {
            let mut temps = backend.temperatures().await?;
            temps.extend(hwmon::read_temperatures(Path::new(hwmon::HWMON_ROOT))?);
            print_temps(&temps);
            Ok(())
        }
        Cmd::Monitor { interval } => monitor(&backend, interval).await,
        Cmd::SetPwm {
            percent,
            allow_low,
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
            backend.set_global_pwm(percent).await?;
            println!("set all PWM channels to {percent}%");
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
    }
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
    let product = fru
        .lines()
        .find_map(|l| l.split_once(':').filter(|(k, _)| k.trim() == "Product Name").map(|(_, v)| v.trim().to_string()));
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
        temps.extend(hwmon::read_temperatures(Path::new(hwmon::HWMON_ROOT))?);
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
    args.iter()
        .map(|s| {
            let s = s.trim();
            if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                u8::from_str_radix(hex, 16).map_err(|e| anyhow::anyhow!("bad index '{s}': {e}"))
            } else {
                s.parse::<u8>().map_err(|e| anyhow::anyhow!("bad index '{s}': {e}"))
            }
        })
        .collect()
}
