# pmgfan

Fujitsu **PRIMERGY TX1320 M4**（iRMC S5）専用のファンコントロールツール群。

iRMC の Fujitsu OEM IPMI コマンドでファン PWM を制御する systemd デーモン
`pmgfand` と、Unix socket 経由で操作する CLI/TUI `pmgfanctl` から構成される。

## 概要

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

設計上の重要な方針:

- **TUI/CLI に root 権限を持たせない**。OEM IPMI 操作はすべてデーモンに限定する
- **異常時は iRMC 標準制御へ必ず戻す**。センサー喪失・IPMI 失敗・デーモン停止時は
  OEM override を解除してフェイルセーフとする
- **`Auto` は「iRMC 本来の制御への復帰」**を意味する。pmgfan 独自の制御モードではない

## 対象ハードウェア

- PRIMERGY TX1320 M4
- iRMC S5（iRMC S5 3.31P / SDR 3.40 で実証済みのプロトコルを利用。他ファームウェアは未保証）
- 起動時に機種・ファームウェアを検証する

## コンポーネント

| 名前 | 役割 |
|---|---|
| `pmgfand` | systemd デーモン。センサー監視・制御・フェイルセーフ・Unix socket API |
| `pmgfanctl` | CLI + TUI。状態表示・カーブ編集・PWM 固定・Target RPM・iRMC Auto 復帰 |

## ドキュメント

| ドキュメント | 内容 |
|---|---|
| [docs/00-overview.md](docs/00-overview.md) | 目的・背景・設計方針 |
| [docs/01-architecture.md](docs/01-architecture.md) | 全体構成・Rust workspace |
| [docs/02-ipmi-backend.md](docs/02-ipmi-backend.md) | IPMI 層・Fujitsu OEM コマンド |
| [docs/03-control.md](docs/03-control.md) | 制御モード・ファンカーブ・Target RPM・キャリブレーション |
| [docs/04-safety.md](docs/04-safety.md) | フェイルセーフ・watchdog・systemd |
| [docs/05-ipc.md](docs/05-ipc.md) | Unix socket API |
| [docs/06-tui.md](docs/06-tui.md) | TUI 設計 |
| [docs/07-cli.md](docs/07-cli.md) | CLI 設計 |
| [docs/08-config.md](docs/08-config.md) | 設定ファイルリファレンス |
| [docs/09-roadmap.md](docs/09-roadmap.md) | 実装フェーズ |

## ステータス

設計段階。実装ロードマップは [docs/09-roadmap.md](docs/09-roadmap.md) を参照。

## ライセンス

[Apache-2.0](LICENSE)
