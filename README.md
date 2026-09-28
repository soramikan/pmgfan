# pmgfan

Fujitsu **PRIMERGY TX1320 M4**（iRMC S5 搭載）専用の高機能・安全重視ファンコントロールツール群。

iRMC の Fujitsu OEM IPMI コマンドでファン PWM を制御する systemd デーモン **`pmgfand`** と、一般ユーザーが Unix ドメインソケット経由で直感的に操作できる CLI / TUI **`pmgfanctl`** から構成されます。

---

## 概要とアーキテクチャ

```text
┌───────────────────────────────────────┐
│              pmgfanctl                │
│                                       │
│  CLI / Ratatui TUI                    │
│  ・リアルタイム監視 (Status)          　　　│
│  ・ファンカーブ制御 (Curve)           　　　│
│  ・PWM 固定設定 (Fixed PWM)           　 │
│  ・目標回転数制御 (Target RPM)        　　 │
│  ・自動キャリブレーション (Calibrate) 　　　　│
│  ・iRMC Auto 復帰                     　│
└───────────────────┬───────────────────┘
                    │
                    │ Unix Domain Socket (/run/pmgfand/control.sock)
                    │ 一般ユーザー権限で操作可能 (pmgfan グループ)
                    ▼
┌───────────────────────────────────────┐
│               pmgfand                 │
│                                       │
│  Sensor Manager (hwmon + IPMI SDR)    │
│  Control Engine (Curve / PI / Sweep)  │
│  Safety Manager (Normal/Degraded/Fail)│
│  IPMI Backend (native ioctl/ipmitool) │
│  systemd Watchdog (10s keepalive)     │
└───────────┬───────────────┬───────────┘
            │               │
            │               └── /sys/class/hwmon/* (CPU/PCH/NVMe 温度)
            │
            └── /dev/ipmi0 (OpenIPMI ioctl または ipmitool)
                   │
                   ▼
               iRMC S5 (Fujitsu OEM 0x2e/0xf5)
             ┌──────────┐
             │ FAN CPU  │ ◄── 制御 (Chassis Scope)
             │ FAN1 SYS │ ◄── 制御 (Chassis Scope)
             │ FAN PSU1 │ ◄── 監視 (iRMC Auto 委任で静音維持)
             │ FAN PSU2 │ ◄── 監視 (iRMC Auto 委任で静音維持)
             └──────────┘
```

---

## 主な特長

- **安全第一のフェイルセーフ設計**:
  センサー値の途絶（10秒）、IPMI 通信エラー、0 RPM（ファン停止）などを検知すると、即座に OEM 強制を解除して **iRMC S5 本来の自動制御へ復帰** します。CPU 90°C / PCH 95°C の緊急温度到達時には全開回転（PWM 100%）でハードウェアを守ります。
- **root 権限分離**:
  ハードウェアを直接操作するデーモンのみが root で動作。日常的に使う CLI や TUI は `pmgfan` グループに所属する一般ユーザーから安全に実行できます。
- **TX1320 M4 に特化した静音化 (`pwm_scope = "chassis"`)**:
  電源ユニット（PSU）ファンの自律特性を考慮し、CPU/ケースファンのみを制御して PSU ファンを iRMC 自動制御に残すことで、PSU ファンの爆音化を防ぎ静音運用を実現します。
- **リッチなターミナル UI (TUI)**:
  `ratatui` を採用。温度・回転数の常時モニタリングや、ファンカーブグラフの表示、キーボードでの直感的なモード変更が可能です。
- **多彩な制御モード**:
  温度連動ファンカーブ（Curve）、固定デューティ比（Fixed PWM）、目標回転数への自動追従（Target RPM PI 制御）、PWM-RPM 自動測定（Calibrate）に対応。
- **Native OpenIPMI 対応**:
  `ipmitool` コマンド経由だけでなく、`/dev/ipmi0` キャラクタデバイスへの ioctl 直接通信バックエンド（Phase 9）を内蔵。

---

## 対象ハードウェア・動作環境

- **対応機種**: Fujitsu PRIMERGY TX1320 M4（iRMC S5 搭載）
- **検証済みファームウェア**: iRMC S5 3.31P / SDR 3.40
- **対応 OS**: Linux（RHEL / AlmaLinux / Rocky Linux 9〜10 等）
- **前提ドライバ**: Linux カーネル OpenIPMI モジュール（`ipmi_si`, `ipmi_devintf`）

---

## クイックスタート

### 1. インストール

#### RPM パッケージを利用する場合（推奨）
```bash
# ビルド
sudo dnf install -y rpm-build systemd-rpm-macros cargo git
./packaging/build-rpm.sh

# インストール
sudo dnf install -y ~/rpmbuild/RPMS/x86_64/pmgfan-*.rpm
```

#### ソースコードから直接ビルドする場合
```bash
cargo build --release
sudo install -m 0755 target/release/pmgfand /usr/sbin/
sudo install -m 0755 target/release/pmgfanctl /usr/bin/
sudo groupadd -r pmgfan 2>/dev/null || true
sudo mkdir -p /etc/pmgfand
sudo cp config/pmgfand.toml /etc/pmgfand/config.toml
sudo cp systemd/pmgfand.service /usr/lib/systemd/system/
sudo systemctl daemon-reload
```

### 2. 一般ユーザーの権限付与
`pmgfanctl` を実行するユーザーを `pmgfan` グループに追加します：
```bash
sudo usermod -aG pmgfan $USER
newgrp pmgfan  # または再ログイン
```

### 3. デーモンの起動
```bash
sudo systemctl enable --now pmgfand
```

### 4. 動作確認
```bash
# 現在の回転数と温度を表示
pmgfanctl status

# TUI を起動（終了は 'q'）
pmgfanctl
```

---

## 日常の操作 (CLI)

`pmgfanctl` サブコマンドで素早く操作できます。

| コマンド | 説明 |
|---|---|
| `pmgfanctl status` | 現在のファン回転数、温度、動作モード、安全状態を表示 |
| `pmgfanctl auto` | OEM 強制を解除し、iRMC S5 本来の自動制御へ戻す |
| `pmgfanctl pwm 30` | シャーシファンを PWM 30% で固定 |
| `pmgfanctl rpm "FAN CPU" 2000` | FAN CPU を 2000 RPM に維持する PI 制御を開始 |
| `pmgfanctl mode curve` | 設定ファイルのファンカーブ自動制御へ移行 |
| `pmgfanctl scope chassis` | 強制範囲をシャーシファンのみに設定（PSU は Auto で静音） |
| `pmgfanctl scope all` | 強制範囲を全ファンに設定（PSU も含む全力冷却） |
| `pmgfanctl calibrate` | PWM 10%〜100% の回転数を自動測定して保存 |

詳細: [CLI リファレンス](docs/guide/03-cli.md)

---

## TUI の操作方法

`pmgfanctl`（または `pmgfanctl tui`）を実行すると、ターミナル上にリアルタイム監視画面が開きます。

| キー | 動作 |
|---|---|
| `q` / `Ctrl+C` | TUI を終了（**バックグラウンドの制御は継続します**） |
| `a` | **iRMC Auto** モードに復帰（OEM 強制解除） |
| `c` | **Curve** モード（ファンカーブ自動制御）へ移行 |
| `f` | **Fixed PWM**（固定値）入力ダイアログを開く |
| `r` | **Target RPM**（目標回転数）入力ダイアログを開く |
| `s` | **Scope**（`chassis` ⇔ `all`）を切り替え |
| `m` | **Mode 選択メニュー**を開く |
| `Esc` | ダイアログを閉じる / キャンセル |

詳細: [TUI 操作ガイド](docs/guide/04-tui.md)

---

## TX1320 M4 静音化のポイント

1. **`pwm_scope = "chassis"`（最重要）**
   TX1320 M4 の電源ユニット（PSU）ファンはハードウェア自律制御を持っています。全ファン強制（`all`）で PWM 30% を指定すると、PSU ファンは約 **5600 RPM** で爆音回転してしまいます。
   `chassis` に設定することで、PSU ファンは iRMC 自動制御（~1600 RPM）に任せ、CPU・ケースファンのみを静かに制御できます。
2. **ハードウェア回転数フロア**
   シャーシファン（FAN CPU / FAN1 SYS）は PWM を 10% や 0% に設定しても停止せず、ハードウェアフロア（約 775 RPM）で安全に回転を継続します。安心して低 PWM を指定できます。

詳細: [設定リファレンスと静音化ガイド](docs/guide/02-configuration.md)

---

## ドキュメント一覧

完全なドキュメントは [**docs/README.md**](docs/README.md) を参照してください。

### 📖 利用者向けガイド
- [インストールとセットアップ](docs/guide/01-install.md)
- [設定リファレンスと静音化ガイド](docs/guide/02-configuration.md)
- [CLI リファレンス](docs/guide/03-cli.md)
- [TUI 操作ガイド](docs/guide/04-tui.md)
- [トラブルシューティング](docs/guide/05-troubleshooting.md)

### 🛠 内部設計・仕様書
- [01. 概要・目的・設計方針](docs/internals/01-overview.md)
- [02. システムアーキテクチャ](docs/internals/02-architecture.md)
- [03. IPMI 層・Fujitsu OEM 仕様](docs/internals/03-ipmi.md)
- [04. ファン制御アルゴリズム](docs/internals/04-control.md)
- [05. 安全機構・フェイルセーフ仕様](docs/internals/05-safety.md)
- [06. Unix Socket IPC 仕様](docs/internals/06-ipc.md)
- [07. 実装ロードマップと検証記録](docs/internals/07-roadmap.md)

---

## ライセンス

[Apache-2.0](LICENSE)
