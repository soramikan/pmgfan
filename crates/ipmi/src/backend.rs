//! `FanControlBackend` 抽象。docs/02-ipmi-backend.md 参照。

use std::io;

use pmgfan_core::control::PwmScope;
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
    #[error("IPMI completion code 0x{0:02x}")]
    Completion(u8),
    #[error("ipmi request timed out after {0}s")]
    Timeout(u64),
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

    /// 強制 PWM（%）を `scope` の範囲に設定する。
    /// `PwmScope::Chassis` では PSU ファンは iRMC 自動制御に残る。
    fn set_pwm(
        &self,
        scope: PwmScope,
        pwm: u8,
    ) -> impl std::future::Future<Output = Result<()>> + Send;

    /// PWM 強制を解除し、iRMC 自動制御へ戻す。
    fn clear_override(&self) -> impl std::future::Future<Output = Result<()>> + Send;

    /// PWM force スロットを読み出す。`indices` は 1..=31 個、各値 0..=31。
    fn read_override_slots(
        &self,
        indices: &[u8],
    ) -> impl std::future::Future<Output = Result<Vec<PwmSlot>>> + Send;
}

/// 設定で選べるバックエンド実装。
/// `native` は `/dev/ipmi0` を ioctl で直接駆動し、`ipmitool` は
/// 従来どおり外部コマンド経由。
pub enum Backend {
    Ipmitool(crate::ipmitool::IpmitoolBackend),
    Native(crate::native::NativeBackend),
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ipmitool(b) => b.fmt(f),
            Self::Native(b) => b.fmt(f),
        }
    }
}

impl FanControlBackend for Backend {
    fn model_name(&self) -> impl std::future::Future<Output = Result<String>> + Send {
        async move {
            match self {
                Self::Ipmitool(b) => b.model_name().await,
                Self::Native(b) => b.model_name().await,
            }
        }
    }

    fn fans(&self) -> impl std::future::Future<Output = Result<Vec<FanReading>>> + Send {
        async move {
            match self {
                Self::Ipmitool(b) => b.fans().await,
                Self::Native(b) => b.fans().await,
            }
        }
    }

    fn temperatures(&self) -> impl std::future::Future<Output = Result<Vec<TempReading>>> + Send {
        async move {
            match self {
                Self::Ipmitool(b) => b.temperatures().await,
                Self::Native(b) => b.temperatures().await,
            }
        }
    }

    fn set_pwm(
        &self,
        scope: PwmScope,
        pwm: u8,
    ) -> impl std::future::Future<Output = Result<()>> + Send {
        async move {
            match self {
                Self::Ipmitool(b) => b.set_pwm(scope, pwm).await,
                Self::Native(b) => b.set_pwm(scope, pwm).await,
            }
        }
    }

    fn clear_override(&self) -> impl std::future::Future<Output = Result<()>> + Send {
        async move {
            match self {
                Self::Ipmitool(b) => b.clear_override().await,
                Self::Native(b) => b.clear_override().await,
            }
        }
    }

    fn read_override_slots(
        &self,
        indices: &[u8],
    ) -> impl std::future::Future<Output = Result<Vec<PwmSlot>>> + Send {
        async move {
            match self {
                Self::Ipmitool(b) => b.read_override_slots(indices).await,
                Self::Native(b) => b.read_override_slots(indices).await,
            }
        }
    }
}
