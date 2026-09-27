//! ファン回転数の読み取りモデル。

/// ファン1台分の読み取り結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FanReading {
    /// SDR 上のセンサー名。例: `FAN CPU`
    pub name: String,
    /// 回転数。`Disabled` / `ns` 等で読めない場合は `None`
    pub rpm: Option<u32>,
    /// SDR ステータス
    pub status: FanStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FanStatus {
    Ok,
    /// `ns` / `Disabled` — スロットは存在するが未搭載または無効
    Disabled,
    /// `nr` — 読み取り不能
    NotPresent,
    /// しきい値アラーム系ステータス（lnc/lcr/lnr/unc/ucr/unr 等）
    Alarm,
    Unknown,
}

impl FanStatus {
    /// ipmitool のステータス文字列から変換する。
    pub fn from_ipmi(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "ok" => Self::Ok,
            "ns" | "disabled" => Self::Disabled,
            "nr" | "np" | "not present" => Self::NotPresent,
            "lnc" | "lcr" | "lnr" | "unc" | "ucr" | "unr" | "cr" | "nc" => Self::Alarm,
            _ => Self::Unknown,
        }
    }

    /// 表示用の短い文字列。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Disabled => "disabled",
            Self::NotPresent => "not-present",
            Self::Alarm => "alarm",
            Self::Unknown => "unknown",
        }
    }
}
