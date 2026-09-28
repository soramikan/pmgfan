//! pmgfanctl — pmgfand の CLI/TUI クライアント。
//!
//! Unix socket 経由で pmgfand を操作する。root 不要
//! （`pmgfan` グループに所属していればよい）。

mod client;
mod tui;

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use pmgfan_core::control::PwmScope;
use pmgfan_core::protocol::{Mode, Request, Response};

#[derive(Parser)]
#[command(
    name = "pmgfanctl",
    version,
    about = "pmgfand control client (CLI / TUI)"
)]
struct Cli {
    /// 制御 socket のパス
    #[arg(long, global = true, default_value_os_t = PathBuf::from(pmgfan_core::protocol::DEFAULT_SOCKET_PATH))]
    socket: PathBuf,
    /// TUI のカーブ表示に使う設定ファイル
    /// （既定: pmgfand と同じ /etc/pmgfand/config.toml）
    #[arg(long, global = true, default_value_os_t = PathBuf::from("/etc/pmgfand/config.toml"))]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// 状態表示（省略時のデフォルト）
    Status,
    /// 監視・操作用 TUI を起動する
    Tui,
    /// iRMC Auto（OEM override 解除）へ戻す
    Auto,
    /// 全 PWM チャンネルを % 固定する
    Pwm {
        /// PWM duty (%)
        percent: u8,
    },
    /// RPM 目標制御（PI フィードバックで参照ファンを目標 RPM に保つ）
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
    /// PWM 強制の適用範囲を切り替える（all = PSU 含む全体 /
    /// chassis = シャーシファンのみで PSU は iRMC 自動制御）
    Scope {
        /// `all` または `chassis`
        scope: String,
    },
    /// PWM→RPM キャリブレーションを開始する。
    /// ファンが min_pwm..100% を順に掃引する（数十秒・音が出る）。
    /// 中断は任意のモード変更（例: `pmgfanctl auto`）で行う。
    /// 結果は /var/lib/pmgfand/calibration.toml に保存され、
    /// Target RPM の初期 PWM 推定に使われる。
    Calibrate,
}

// multi_thread が必須: TUI のイベントループは crossterm の
// 同期 poll/draw で await しないため、current_thread だと
// バックグラウンドの socket 要求タスクが永久に実行されない。
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    // Rust はデフォルトで SIGPIPE を無視するため、パイプ切断時に
    // println! が panic する。CLI としては従来どおり SIG_DFL で
    // 静かに終了させる（head 等との組み合わせ対策）。
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let cli = Cli::parse();
    // 引数なし: 対話端末（stdin/stdout ともに TTY）なら TUI、
    // パイプ等なら status（スクリプトからの出力を壊さない）
    let cmd = cli.cmd.unwrap_or_else(|| {
        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            Cmd::Tui
        } else {
            Cmd::Status
        }
    });
    match cmd {
        Cmd::Tui => tui::run(&cli.socket, Some(&cli.config)).await,
        Cmd::Status => {
            let resp = client::request(&cli.socket, &Request::GetStatus).await?;
            match resp {
                Response::Status {
                    state,
                    mode,
                    pwm,
                    pwm_scope,
                    calibration,
                    fans,
                    temperatures,
                    uptime_secs,
                } => print_status(
                    state,
                    &mode,
                    pwm,
                    pwm_scope,
                    calibration.as_ref(),
                    &fans,
                    &temperatures,
                    uptime_secs,
                ),
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
        Cmd::Scope { scope } => {
            let scope = PwmScope::parse(&scope).map_err(anyhow::Error::msg)?;
            let resp = client::request(&cli.socket, &Request::SetPwmScope { scope }).await?;
            client::expect_ok(resp)?;
            println!("pwm scope -> {}", scope.as_str());
            Ok(())
        }
        Cmd::Calibrate => {
            let resp = client::request(&cli.socket, &Request::StartCalibration).await?;
            client::expect_ok(resp)?;
            println!("calibration started — watch progress with `pmgfanctl status`");
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn print_status(
    state: pmgfan_core::protocol::DaemonState,
    mode: &Mode,
    pwm: Option<u8>,
    pwm_scope: PwmScope,
    calibration: Option<&pmgfan_core::protocol::CalibStatus>,
    fans: &[pmgfan_core::fan::FanReading],
    temps: &[pmgfan_core::sensor::TempReading],
    uptime_secs: f64,
) {
    let mode_str = match mode {
        Mode::IrmcAuto => "iRMC Auto".to_string(),
        Mode::FixedPwm(p) => format!("Fixed PWM {p}%"),
        Mode::Curve => "Curve".to_string(),
        Mode::TargetRpm { fan, rpm } => format!("Target {rpm} RPM ({fan})"),
        Mode::Calibrate => "Calibrating".to_string(),
    };
    println!("Mode: {mode_str}");
    println!("State: {:?}   Uptime: {:.0}s", state, uptime_secs);
    if let Some(p) = pwm {
        println!("PWM: {p}%  (scope: {})", pwm_scope.as_str());
    } else {
        println!("Scope: {}", pwm_scope.as_str());
    }
    if let Some(c) = calibration {
        if c.active {
            println!(
                "Calibration: step {}/{} (sweeping {}%)",
                c.step,
                c.total,
                c.current_pwm
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "-".into())
            );
        } else if let Some(r) = &c.result {
            println!("Calibration: {r}");
        }
        for p in &c.points {
            let mut s = format!("  pwm={:>3}%", p.pwm);
            for (name, rpm) in &p.rpm {
                s.push_str(&format!("  {name}={rpm}"));
            }
            println!("{s}");
        }
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
