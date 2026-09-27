# 01. アーキテクチャ

## 全体構成

```
┌────────────────────────────┐
│        pmgfanctl           │
│                            │
│  TUI / CLI                 │
│  ・状態表示                 │
│  ・Curve編集                │
│  ・PWM固定                  │
│  ・Target RPM              │
│  ・iRMC Auto復帰            │
└────────────┬───────────────┘
             │
             │ Unix socket
             │ /run/pmgfand/control.sock
             ▼
┌────────────────────────────┐
│         pmgfand            │
│                            │
│ Sensor Manager             │
│ Control Engine             │
│ Safety Manager             │
│ IPMI Backend               │
│ systemd watchdog           │
└──────┬─────────┬───────────┘
       │         │
       │         └── /sys/class/hwmon/*
       │               CPU/PCH/NVMe等
       │
       └── /dev/ipmi0
              │
              ▼
          iRMC S5
        ┌──────────┐
        │ FAN CPU  │
        │ FAN1 SYS │
        │ FAN PSU1 │
        │ FAN PSU2 │
        └──────────┘
```

- **pmgfand**: 全状態・全制御・IPMI 通信を持つ。root として動作
- **pmgfanctl**: 非 root。`pmgfan` グループ経由で socket にアクセス

## Rust workspace

```text
pmgfan/
├── Cargo.toml
├── crates/
│   ├── core/
│   │   ├── curve.rs
│   │   ├── control.rs
│   │   ├── sensor.rs
│   │   ├── config.rs
│   │   └── protocol.rs
│   │
│   ├── ipmi/
│   │   ├── backend.rs
│   │   ├── ipmitool.rs
│   │   └── fujitsu.rs
│   │
│   ├── daemon/
│   │   ├── main.rs
│   │   ├── manager.rs
│   │   ├── safety.rs
│   │   ├── ipc.rs
│   │   └── watchdog.rs
│   │
│   └── tui/
│       ├── main.rs
│       ├── app.rs
│       ├── ui.rs
│       └── widgets/
│
├── config/
│   └── pmgfand.toml
└── systemd/
    └── pmgfand.service
```

| crate | 役割 |
|---|---|
| `core` | カーブ評価・制御ロジック・設定・IPC プロトコル型。OS/IPMI 非依存でテスト可能にする |
| `ipmi` | `FanControlBackend` trait と ipmitool/Fujitsu OEM 実装 |
| `daemon` | pmgfand バイナリ。マネージャ・セーフティ状態機械・IPC・watchdog |
| `tui` | pmgfanctl バイナリ。ratatui アプリ |

## 採用クレート

```text
tokio
serde
serde_json
toml
clap
thiserror
anyhow
tracing
tracing-journald

ratatui
crossterm
```

独自 OpenIPMI ioctl 実装は初期は書かない。v1 は `ipmitool` プロセス経由
（[02-ipmi-backend.md](02-ipmi-backend.md)）。

## 温度取得

ファン RPM は iRMC（IPMI SDR）が最も信頼できる情報源。

温度は毎回 `sensors` を spawn せず、Rust から直接 hwmon を読む:

```text
/sys/class/hwmon/hwmon*/name
/sys/class/hwmon/hwmon*/temp*_input
/sys/class/hwmon/hwmon*/temp*_label
```

想定ソース:

```text
coretemp
pch_cannonlake
nvme
```

加えて IPMI 側温度も `ipmitool -I open -c sdr type temperature` で取得し、
**Linux hwmon + iRMC センサーの両方**を扱えるようにする。
