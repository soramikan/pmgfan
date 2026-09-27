//! `ipmitool` プロセス経由のバックエンド。
//!
//! - SDR 読み取り: `ipmitool -I <iface> -c sdr type {fan,temperature}` の CSV 出力
//! - OEM 制御: `ipmitool -I <iface> raw 0x2e 0xf5 <data...>`

use std::ffi::OsString;
use std::fmt;

use pmgfan_core::fan::{FanReading, FanStatus};
use pmgfan_core::sensor::TempReading;
use tokio::process::Command;

use crate::backend::{FanControlBackend, IpmiError, PwmSlot, Result};
use crate::fujitsu;

/// 1回の ipmitool 呼び出しの上限。これを超えると子プロセスを kill して
/// エラーにする（デーモン側で無制限に待機しないための保証）。
const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

pub struct IpmitoolBackend {
    bin: OsString,
    interface: String,
}

impl fmt::Debug for IpmitoolBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IpmitoolBackend")
            .field("bin", &self.bin)
            .field("interface", &self.interface)
            .finish()
    }
}

impl IpmitoolBackend {
    pub fn new(bin: impl Into<OsString>, interface: impl Into<String>) -> Self {
        Self {
            bin: bin.into(),
            interface: interface.into(),
        }
    }

    /// `ipmitool -I <iface> <args...>` を実行し stdout を返す。
    async fn output(&self, args: &[String]) -> Result<String> {
        let mut cmdline = format!("{:?} -I {}", self.bin, self.interface);
        for a in args {
            cmdline.push(' ');
            cmdline.push_str(a);
        }
        let mut cmd = Command::new(&self.bin);
        cmd.arg("-I")
            .arg(&self.interface)
            .args(args)
            // タイムアウト時に future が drop されても子プロセスが残らないように
            .kill_on_drop(true);
        let out = match tokio::time::timeout(COMMAND_TIMEOUT, cmd.output()).await {
            Ok(out) => out.map_err(|e| IpmiError::Spawn {
                cmd: cmdline.clone(),
                source: e,
            })?,
            Err(_) => {
                return Err(IpmiError::Command {
                    cmd: cmdline,
                    stderr: format!("timed out after {}s", COMMAND_TIMEOUT.as_secs()),
                });
            }
        };
        if !out.status.success() {
            return Err(IpmiError::Command {
                cmd: cmdline,
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// OEM raw コマンドを送り、応答をバイト列で返す。
    async fn raw(&self, data: &[u8]) -> Result<Vec<u8>> {
        let mut args = vec![
            "raw".into(),
            format!("0x{:02x}", fujitsu::NETFN),
            format!("0x{:02x}", fujitsu::COMMAND),
        ];
        args.extend(data.iter().map(|b| format!("0x{b:02x}")));
        let stdout = self.output(&args).await?;
        Ok(fujitsu::parse_hex_bytes(&stdout))
    }

    /// `ipmitool fru print` の生テキスト。
    pub async fn fru(&self) -> Result<String> {
        self.output(&["fru".into(), "print".into()]).await
    }

    /// `ipmitool mc info` の生テキスト。
    pub async fn mc_info(&self) -> Result<String> {
        self.output(&["mc".into(), "info".into()]).await
    }
}

impl FanControlBackend for IpmitoolBackend {
    fn model_name(&self) -> impl std::future::Future<Output = Result<String>> + Send {
        async move {
            let fru = self.fru().await?;
            fru.lines()
                .find_map(|l| {
                    l.split_once(':')
                        .filter(|(k, _)| k.trim() == "Product Name")
                        .map(|(_, v)| v.trim().to_string())
                })
                .ok_or_else(|| IpmiError::Parse("Product Name not found in fru".into()))
        }
    }

    fn fans(&self) -> impl std::future::Future<Output = Result<Vec<FanReading>>> + Send {
        async move {
            let out = self
                .output(&["-c".into(), "sdr".into(), "type".into(), "fan".into()])
                .await?;
            Ok(parse_fans(&out))
        }
    }

    fn temperatures(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<TempReading>>> + Send {
        async move {
            let out = self
                .output(&[
                    "-c".into(),
                    "sdr".into(),
                    "type".into(),
                    "temperature".into(),
                ])
                .await?;
            Ok(parse_temperatures(&out))
        }
    }

    fn set_global_pwm(&self, pwm: u8) -> impl std::future::Future<Output = Result<()>> + Send {
        async move {
            if pwm > fujitsu::MAX_PWM {
                return Err(IpmiError::Parse(format!("pwm must be 0..=100, got {pwm}")));
            }
            self.raw(&fujitsu::set_global_pwm_data(pwm)).await?;
            Ok(())
        }
    }

    fn clear_override(&self) -> impl std::future::Future<Output = Result<()>> + Send {
        async move {
            self.raw(&fujitsu::clear_override_data()).await?;
            Ok(())
        }
    }

    fn read_override_slots(
        &self,
        indices: &[u8],
    ) -> impl std::future::Future<Output = Result<Vec<PwmSlot>>> + Send {
        let indices = indices.to_vec();
        async move {
            let resp = self.raw(&fujitsu::read_slots_data(&indices)?).await?;
            Ok(fujitsu::decode_slots(&resp, &indices))
        }
    }
}

/// `sdr type <x> -c` の1行。
/// - 4フィールド: `name,value,unit,status`（コンパクト形式）
/// - 5フィールド以上: `name,<hex>id,status,entity,reading`（elist 形式・離散センサー等）
enum SdrRow<'a> {
    Compact {
        name: &'a str,
        value: &'a str,
        status: &'a str,
    },
    Elist {
        name: &'a str,
        status: &'a str,
        reading: &'a str,
    },
}

fn parse_row(line: &str) -> Option<SdrRow<'_>> {
    let f: Vec<&str> = line.split(',').map(str::trim).collect();
    match f.len() {
        4 => Some(SdrRow::Compact {
            name: f[0],
            value: f[1],
            status: f[3],
        }),
        n if n >= 5 => Some(SdrRow::Elist {
            name: f[0],
            status: f[2],
            reading: f[4],
        }),
        _ => None,
    }
}

/// `"2875 RPM"` / `"44 degrees C"` の先頭の数値を取り出す。
fn leading_number(s: &str) -> Option<f64> {
    s.split_whitespace().next()?.parse().ok()
}

/// `-c sdr type fan` の CSV を `FanReading` に変換する。
pub fn parse_fans(csv: &str) -> Vec<FanReading> {
    csv.lines()
        .filter_map(|line| {
            let row = parse_row(line)?;
            let (name, rpm, status) = match row {
                SdrRow::Compact { name, value, status } => {
                    (name, value.parse::<u32>().ok(), status)
                }
                SdrRow::Elist {
                    name,
                    status,
                    reading,
                } => (name, leading_number(reading).map(|v| v as u32), status),
            };
            if name.is_empty() {
                return None;
            }
            Some(FanReading {
                name: name.to_string(),
                rpm,
                status: FanStatus::from_ipmi(status),
            })
        })
        .collect()
}

/// `-c sdr type temperature` の CSV を `TempReading` に変換する。
/// 数値を持たない行（"Device Present" 等のイベント専用センサー）は捨てる。
pub fn parse_temperatures(csv: &str) -> Vec<TempReading> {
    csv.lines()
        .filter_map(|line| {
            let row = parse_row(line)?;
            let (name, celsius) = match row {
                SdrRow::Compact { name, value, .. } => (name, value.parse::<f64>().ok()?),
                SdrRow::Elist { name, reading, .. } => (name, leading_number(reading)?),
            };
            if name.is_empty() {
                return None;
            }
            Some(TempReading {
                chip: "ipmi".to_string(),
                label: name.to_string(),
                celsius,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pmgfan_core::fan::FanStatus;

    const FAN_CSV: &str = "\
FAN CPU,3125,RPM,ok
FAN1 SYS,2650,RPM,ok
FAN2 SYS,,,ns
FAN PSU,,,ns
FAN PSU1,3760,RPM,ok
FAN PSU2,1680,RPM,ok
";

    #[test]
    fn parse_fans_real_output() {
        let fans = parse_fans(FAN_CSV);
        assert_eq!(fans.len(), 6);
        assert_eq!(fans[0].name, "FAN CPU");
        assert_eq!(fans[0].rpm, Some(3125));
        assert_eq!(fans[0].status, FanStatus::Ok);
        assert_eq!(fans[2].rpm, None);
        assert_eq!(fans[2].status, FanStatus::Disabled);
        assert_eq!(fans[5].name, "FAN PSU2");
        assert_eq!(fans[5].rpm, Some(1680));
    }

    #[test]
    fn parse_fans_elist_format() {
        let fans = parse_fans("FAN CPU,64h,ok,7.1,2875 RPM\n");
        assert_eq!(fans[0].rpm, Some(2875));
        assert_eq!(fans[0].status, FanStatus::Ok);
    }

    const TEMP_CSV: &str = "\
Ambient,30,degrees C,ok
CPU,36,degrees C,ok
FBU,,,ns
PCH,53,degrees C,ok
Ambient,36h,ok,55.0,Device Present
";

    #[test]
    fn parse_temperatures_real_output() {
        let temps = parse_temperatures(TEMP_CSV);
        assert_eq!(temps.len(), 3);
        assert_eq!(temps[0].label, "Ambient");
        assert_eq!(temps[0].celsius, 30.0);
        assert!(temps.iter().any(|t| t.label == "PCH" && t.celsius == 53.0));
        // 数値のない行・離散行は除外される
        assert!(!temps.iter().any(|t| t.label == "FBU"));
    }
}
