//! Unix socket IPC プロトコル。docs/05-ipc.md 参照。
//!
//! 1接続1行の JSON Lines。リクエスト/レスポンスともに
//! `"version": 1` と `"type"` タグを持つ。

use serde::{Deserialize, Serialize};

use crate::fan::FanReading;
use crate::sensor::TempReading;

pub const PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_SOCKET_PATH: &str = "/run/pmgfand/control.sock";

/// デーモンの制御モード。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// OEM override 解除 = iRMC 本来の制御へ戻す
    IrmcAuto,
    /// 全 PWM チャンネルを % 固定
    FixedPwm(u8),
    /// 温度カーブ制御（Phase 4）
    Curve,
    /// RPM フィードバック制御（Phase 7）
    TargetRpm { fan: String, rpm: u32 },
}

/// デーモンの状態機械。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonState {
    Starting,
    Monitoring,
    Controlling,
    Degraded,
    Failsafe,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    GetStatus,
    SetMode { mode: Mode },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Status {
        state: DaemonState,
        mode: Mode,
        /// 現在強制している PWM（未強制なら None）
        pwm: Option<u8>,
        fans: Vec<FanReading>,
        temperatures: Vec<TempReading>,
        uptime_secs: f64,
    },
    Ok,
    Error {
        error: String,
    },
}

/// JSON へ `"version"` フィールドを付与して1行にエンコードする。
/// デコード側は未知フィールドを無視するため version はオプション扱い。
pub fn encode<T: Serialize>(msg: &T) -> serde_json::Result<String> {
    let mut v = serde_json::to_value(msg)?;
    if let Some(obj) = v.as_object_mut() {
        obj.insert("version".into(), PROTOCOL_VERSION.into());
    }
    Ok(v.to_string())
}

pub fn decode_request(line: &str) -> Result<Request, serde_json::Error> {
    serde_json::from_str(line)
}

pub fn decode_response(line: &str) -> Result<Response, serde_json::Error> {
    serde_json::from_str(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_get_status_with_version_and_type() {
        let s = encode(&Request::GetStatus).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["type"], "get_status");
    }

    #[test]
    fn set_mode_fixed_pwm_wire_format() {
        let s = encode(&Request::SetMode {
            mode: Mode::FixedPwm(40),
        })
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["type"], "set_mode");
        assert_eq!(v["mode"], serde_json::json!({"fixed_pwm": 40}));
    }

    #[test]
    fn set_mode_wire_formats_match_design() {
        // docs/05-ipc.md の例と一致すること
        let auto = serde_json::to_value(Mode::IrmcAuto).unwrap();
        assert_eq!(auto, serde_json::json!("irmc_auto"));
        let rpm = serde_json::to_value(Mode::TargetRpm {
            fan: "FAN CPU".into(),
            rpm: 2500,
        })
        .unwrap();
        assert_eq!(rpm, serde_json::json!({"target_rpm": {"fan": "FAN CPU", "rpm": 2500}}));
    }

    #[test]
    fn decodes_design_example_request() {
        let req = decode_request(r#"{"version":1,"type":"get_status"}"#).unwrap();
        assert!(matches!(req, Request::GetStatus));
        let req =
            decode_request(r#"{"type":"set_mode","mode":{"target_rpm":{"fan":"FAN CPU","rpm":2500}}}"#)
                .unwrap();
        assert!(matches!(
            req,
            Request::SetMode {
                mode: Mode::TargetRpm { rpm: 2500, .. }
            }
        ));
    }
}
