//! `FanControlBackend` 抽象。docs/02-ipmi-backend.md 参照。

use std::io;

use pmgfan_core::fan::FanReading;
use pmgfan_core::sensor::TempReading;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum IpmiError {
    #[error("failed to spawn `{cmd}`: {source}")]
    Spawn { cmd: String, source: io::Error },
    #[error("`{cmd}` failed: {stderr}")]
    Command { cmd: String, stderr: String },
    #[error("unexpected ipmitool output: {0}")]
    Parse(String),
    #[error("operation not supported by this backend")]
    Unsupported,
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, IpmiError>;

/// PWM force スロット1つ分の読み出し。
///
/// 実機観測: 応答ペアは `(index|flags, value)`。
/// 先頭バイトの下位6bit がスロット index、bit7 が強制フラグ。
/// 例: 強制40%時 `(0xc0, 0x28)`、自動時 `(0x40, 0x59)`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PwmSlot {
    /// スロット index（応答バイトの下位6bit）
    pub index: u8,
    /// bit7 = 強制中
    pub forced: bool,
    /// スロット値。強制時は PWM%（実機観測: 0x28 = 40%）
    pub value: u8,
}

/// ファン制御バックエンドの抽象。
/// v1 は `IpmitoolBackend`、将来 `/dev/ipmi0` 直接アクセスや
/// テスト用 Mock を差し替え可能にする。
///
/// `async fn` ではなく `impl Future + Send` 形にして、呼び出し側が
/// tokio の Send コンテキストで使えることを保証する。
pub trait FanControlBackend {
    /// FRU の製品名（例: "PRIMERGY TX1320 M4"）。起動時の機種検証に使う。
    fn model_name(&self) -> impl std::future::Future<Output = Result<String>> + Send;

    /// ファン SDR を読む。
    fn fans(&self) -> impl std::future::Future<Output = Result<Vec<FanReading>>> + Send;

    /// 温度 SDR を読む。
    fn temperatures(&self) -> impl std::future::Future<Output = Result<Vec<TempReading>>> + Send;

    /// 全 PWM チャンネル（0xff）に強制 PWM（%）を設定する。
    fn set_global_pwm(&self, pwm: u8) -> impl std::future::Future<Output = Result<()>> + Send;

    /// PWM 強制を解除し、iRMC 自動制御へ戻す。
    fn clear_override(&self) -> impl std::future::Future<Output = Result<()>> + Send;

    /// PWM force スロットを読み出す。`indices` は 1..=31 個、各値 0..=31。
    fn read_override_slots(
        &self,
        indices: &[u8],
    ) -> impl std::future::Future<Output = Result<Vec<PwmSlot>>> + Send;
}
