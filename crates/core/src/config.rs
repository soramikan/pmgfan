//! 設定ファイルモデル。docs/guide/02-configuration.md 参照。
//! TOML へのデシリアライズは serde のみに留め、toml クレートへの
//! 依存は呼び出し側（daemon）に限定する。

use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub device: DeviceConfig,
    pub monitor: MonitorConfig,
    pub control: ControlConfig,
    pub safety: SafetyConfig,
    #[serde(rename = "curve")]
    pub curves: Vec<CurveConfig>,
    pub target_rpm: Option<TargetRpmConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DeviceConfig {
    pub model: String,
    /// `ipmitool`（外部コマンド経由）または `native`（/dev/ipmi0 直接 ioctl）
    pub backend: String,
    /// backend = "ipmitool" のときの `-I` インターフェース名
    pub interface: String,
    /// backend = "native" のときの IPMI デバイスパス
    pub path: String,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            model: "PRIMERGY TX1320 M4".into(),
            backend: "ipmitool".into(),
            interface: "open".into(),
            path: "/dev/ipmi0".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MonitorConfig {
    pub fan_interval_ms: u64,
    pub temperature_interval_ms: u64,
    pub history_seconds: u64,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            fan_interval_ms: 2000,
            temperature_interval_ms: 1000,
            history_seconds: 900,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ControlConfig {
    /// `auto` / `irmc_auto` / `fixed_pwm` / `curve` / `target_rpm`
    pub mode: String,
    /// mode = fixed_pwm のときの値
    pub fixed_pwm: Option<u8>,
    pub min_pwm: u8,
    pub max_pwm: u8,
    /// 強制 PWM の適用範囲: "all"（全ファン）| "chassis"（PSU は Auto のまま）
    pub pwm_scope: String,
    pub step_up: u8,
    pub step_down: u8,
    pub down_hysteresis: u8,
    pub min_apply_interval_ms: u64,
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            mode: "auto".into(),
            fixed_pwm: None,
            min_pwm: 30,
            max_pwm: 100,
            pwm_scope: "all".into(),
            step_up: 20,
            step_down: 5,
            down_hysteresis: 5,
            min_apply_interval_ms: 5000,
        }
    }
}

impl ControlConfig {
    pub fn apply_interval(&self) -> Duration {
        Duration::from_millis(self.min_apply_interval_ms.max(200))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SafetyConfig {
    pub sensor_stale_seconds: u64,
    pub ipmi_failure_limit: u32,
    pub cpu_emergency: f32,
    pub pch_emergency: f32,
    pub fail_action: String,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            sensor_stale_seconds: 10,
            ipmi_failure_limit: 3,
            cpu_emergency: 90.0,
            pch_emergency: 95.0,
            fail_action: "irmc-auto".into(),
        }
    }
}

/// `[[curve]]` セクション。`points` は `[[t,p],...]` または
/// `[{temp=t,pwm=p},...]` のどちらでも書ける。
#[derive(Debug, Clone, Deserialize)]
pub struct CurveConfig {
    pub sensor: String,
    #[serde(deserialize_with = "deserialize_points")]
    pub points: Vec<(f32, f32)>,
}

fn deserialize_points<'de, D>(d: D) -> Result<Vec<(f32, f32)>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Point {
        Pair(f32, f32),
        Named { temp: f32, pwm: f32 },
    }
    let pts = Vec::<Point>::deserialize(d)?;
    Ok(pts
        .into_iter()
        .map(|p| match p {
            Point::Pair(t, w) => (t, w),
            Point::Named { temp, pwm } => (temp, pwm),
        })
        .collect())
}

/// `[target_rpm]` セクション。PI ゲインとデフォルトの目標値。
/// `min_pwm`/`max_pwm` は未指定なら `[control]` の範囲を使う。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TargetRpmConfig {
    /// 目標 RPM の基準ファン（SDR のファン名。例: "FAN CPU"）
    pub reference_fan: String,
    /// `mode = "target_rpm"` で起動した場合の目標 RPM
    /// （TUI/CLI からの切替では set_mode が値を持つ）
    pub target: u32,
    pub kp: f32,
    pub ki: f32,
    /// |誤差| がこの RPM 以内なら PWM を変えない
    pub deadband: f32,
    /// PI 出力の範囲（未指定なら [control] min/max に従う）
    pub min_pwm: Option<u8>,
    pub max_pwm: Option<u8>,
}

impl Default for TargetRpmConfig {
    fn default() -> Self {
        Self {
            reference_fan: "FAN CPU".into(),
            target: 2500,
            kp: 0.003,
            ki: 0.0001,
            deadband: 75.0,
            min_pwm: None,
            max_pwm: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_design_values() {
        let c = Config::default();
        assert_eq!(c.control.min_pwm, 30);
        assert_eq!(c.control.max_pwm, 100);
        assert_eq!(c.control.step_up, 20);
        assert_eq!(c.control.step_down, 5);
        assert_eq!(c.control.down_hysteresis, 5);
        assert_eq!(c.control.min_apply_interval_ms, 5000);
        assert_eq!(c.monitor.fan_interval_ms, 2000);
        assert_eq!(c.device.model, "PRIMERGY TX1320 M4");
    }
}
