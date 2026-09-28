//! iRMC / Fujitsu OEM IPMI バックエンド。

// `FanControlBackend` は Send 境界を明示するため `impl Future + Send`
// 形式を意図的に使う（manual_async_fn の既定形）
#![allow(clippy::manual_async_fn)]

pub mod backend;
pub mod fujitsu;
pub mod ipmitool;
pub mod native;
