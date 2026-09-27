//! pmgfanctl — pmgfand の CLI/TUI クライアント（Phase 2: CLI のみ）。
//!
//! Unix socket 経由で pmgfand を操作する。root 不要
//! （`pmgfan` グループに所属していればよい）。

mod client;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use pmgfan_core::protocol::{Mode, Request, Response};

#[derive(Parser)]
#[command(
    name = "pmgfanctl",
    version,
    about = "pmgfand control client (CLI; TUI arrives in a later phase)"
)]
struct Cli {
    /// 制御 socket のパス
    #[arg(long, global = true, default_value_os_t = PathBuf::from(pmgfan_core::protocol::DEFAULT_SOCKET_PATH))]
    socket: PathBuf,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// 状態表示（省略時のデフォルト）
    Status,
    /// iRMC Auto（OEM override 解除）へ戻す
    Auto,
    /// 全 PWM チャンネルを % 固定する
    Pwm {
        /// PWM duty (%)
        percent: u8,
    },
    /// RPM 目標制御（Phase 7。現状は pmgfand がエラーを返す）
    Rpm {
        /// 基準ファン名（例: "FAN CPU"）
        fan: String,
        /// 目標 RPM
        rpm: u32,
    },
    /// モード指定（auto / curve）
    Mode {
        /// `auto` = iRMC 復帰、`curve` = ファンカーブ（Phase 4）
        mode: String,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // Rust はデフォルトで SIGPIPE を無視するため、パイプ切断時に
    // println! が panic する。CLI としては従来どおり SIG_DFL で
    // 静かに終了させる（head 等との組み合わせ対策）。
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let cli = Cli::parse();
    match cli.cmd.unwrap_or(Cmd::Status) {
        Cmd::Status => {
            let resp = client::request(&cli.socket, &Request::GetStatus).await?;
            match resp {
                Response::Status {
                    state,
                    mode,
                    pwm,
                    fans,
                    temperatures,
                    uptime_secs,
                } => print_status(state, &mode, pwm, &fans, &temperatures, uptime_secs),
                Response::Error { error } => anyhow::bail!("pmgfand: {error}"),
                _ => anyhow::bail!("unexpected response"),
            }
            Ok(())
        }
        Cmd::Auto => {
            let resp = client::request(
                &cli.socket,
                &Request::SetMode {
                    mode: Mode::IrmcAuto,
                },
            )
            .await?;
            client::expect_ok(resp)?;
            println!("iRMC automatic control restored");
            Ok(())
        }
        Cmd::Pwm { percent } => {
            let resp = client::request(
                &cli.socket,
                &Request::SetMode {
                    mode: Mode::FixedPwm(percent),
                },
            )
            .await?;
            client::expect_ok(resp)?;
            println!("fixed PWM set to {percent}%");
            Ok(())
        }
        Cmd::Rpm { fan, rpm } => {
            let resp = client::request(
                &cli.socket,
                &Request::SetMode {
                    mode: Mode::TargetRpm { fan, rpm },
                },
            )
            .await?;
            client::expect_ok(resp)?;
            Ok(())
        }
        Cmd::Mode { mode } => {
            let mode = match mode.as_str() {
                "auto" | "irmc_auto" => Mode::IrmcAuto,
                "curve" => Mode::Curve,
                other => anyhow::bail!("unknown mode '{other}' (expected: auto | curve)"),
            };
            let resp = client::request(&cli.socket, &Request::SetMode { mode }).await?;
            client::expect_ok(resp)?;
            println!("mode applied");
            Ok(())
        }
    }
}

fn print_status(
    state: pmgfan_core::protocol::DaemonState,
    mode: &Mode,
    pwm: Option<u8>,
    fans: &[pmgfan_core::fan::FanReading],
    temps: &[pmgfan_core::sensor::TempReading],
    uptime_secs: f64,
) {
    let mode_str = match mode {
        Mode::IrmcAuto => "iRMC Auto".to_string(),
        Mode::FixedPwm(p) => format!("Fixed PWM {p}%"),
        Mode::Curve => "Curve".to_string(),
        Mode::TargetRpm { fan, rpm } => format!("Target {rpm} RPM ({fan})"),
    };
    println!("Mode: {mode_str}");
    println!("State: {:?}   Uptime: {:.0}s", state, uptime_secs);
    if let Some(p) = pwm {
        println!("PWM: {p}%");
    }
    println!();
    for f in fans {
        match f.rpm {
            Some(rpm) => println!("{:<12} {rpm:>5} RPM   {}", f.name, f.status.as_str()),
            None => println!("{:<12} {:>5}       {}", f.name, "--", f.status.as_str()),
        }
    }
    println!();
    for t in temps {
        println!("{:<16} {:<20} {:>5.1} C", t.chip, t.label, t.celsius);
    }
}
