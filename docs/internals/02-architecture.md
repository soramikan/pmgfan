# 02. システムアーキテクチャ

## 全体構成

```text
┌───────────────────────────────────────┐
│              pmgfanctl                │
│                                       │
│  CLI / Ratatui TUI                    │
│  ・状態表示 (Status)                  │
│  ・ファンカーブ監視 (Curve)           │
│  ・固定 PWM 設定 (Fixed PWM)          │
│  ・目標 RPM 追従 (Target RPM)         │
│  ・自動キャリブレーション (Calibrate) │
│  ・iRMC Auto 復帰                     │
└───────────────────┬───────────────────┘
                    │
                    │ Unix Domain Socket
                    │ /run/pmgfand/control.sock (0750, root:pmgfan)
                    ▼
┌───────────────────────────────────────┐
│               pmgfand                 │
│                                       │
│  Sensor Manager                       │
│  Control Engine (Curve / PI / Sweep)  │
│  Safety Manager (Failsafe / Degraded) │
│  IPMI Backend (native / ipmitool)     │
│  systemd Watchdog (10s keepalive)     │
└───────────┬───────────────┬───────────┘
            │               │
            │               └── /sys/class/hwmon/*
            │                     CPU / PCH / NVMe 等
            │
            └── /dev/ipmi0 (OpenIPMI ioctl または ipmitool)
                   │
                   ▼
               iRMC S5 (KCS)
             ┌──────────┐
             │ FAN CPU  │ ◄── 制御 (Chassis Scope)
             │ FAN1 SYS │ ◄── 制御 (Chassis Scope)
             │ FAN PSU1 │ ◄── 監視 (iRMC Auto / Optional All Scope)
             │ FAN PSU2 │ ◄── 監視 (iRMC Auto / Optional All Scope)
             └──────────┘
```

- **pmgfand**: 全制御状態、安全ステートマシン、IPMI トランスポートを保持。root 権限で常駐実行。
- **pmgfanctl**: 一般ユーザーが実行。`pmgfan` グループパーミッションにより Unix ドメインソケット経由で安全に通信。

---

## Rust Workspace 構成

```text
pmgfan/
├── Cargo.toml
├── crates/
│   ├── core/           # ドメインロジック・プロトコル
│   │   ├── config.rs   # 設定ファイル (config.toml) モデル・バリデーション
│   │   ├── control.rs  # Target RPM PI コントローラ・スループット制御
│   │   ├── curve.rs    # 温度ファンカーブの区分線形補間
│   │   ├── protocol.rs # Unix socket IPC リクエスト / レスポンス型
│   │   └── sensor.rs   # センサー名解決・エイリアスマッピング
│   │
│   ├── ipmi/           # IPMI トランスポート・OEM プロトコル
│   │   ├── backend.rs  # FanControlBackend trait 抽象
│   │   ├── fujitsu.rs  # Fujitsu OEM ペイロード生成・解析
│   │   ├── ipmitool.rs # ipmitool プロセス実行バックエンド
│   │   ├── native.rs   # /dev/ipmi0 OpenIPMI ioctl 直接通信バックエンド
│   │   └── mock.rs     # 単体テスト用モックバックエンド
│   │
│   ├── daemon/         # pmgfand デーモンバイナリ
│   │   ├── main.rs     # CLI 引数・初期化・シグナルハンドラ
│   │   ├── daemon.rs   # メインループ・センサー監視・制御ディスパッチ
│   │   ├── safety.rs   # セーフティ状態機械 (Normal / Degraded / Failsafe)
│   │   ├── ipc.rs      # Unix ドメインソケット接続管理・ハンドラ
│   │   └── watchdog.rs # systemd notify watchdog ハートビート
│   │
│   └── tui/            # pmgfanctl CLI/TUI バイナリ
│       ├── main.rs     # サブコマンドルーティング (status, pwm, rpm, tui 等)
│       ├── app.rs      # TUI アプリケーションステート・キーイベントループ
│       ├── ui.rs       # Ratatui 描画レイアウト
│       └── widgets/    # ゲージ・グラフ・ダイアログ等の各コンポーネント
│
├── config/
│   └── pmgfand.toml    # 設定ファイルのマスターテンプレート
├── systemd/
│   └── pmgfand.service # systemd サービス定義ファイル
└── packaging/          # RPM ビルドスクリプト & spec ファイル
```

### クレートの分離原則

- **`pmgfan_core`**:
  OS や IPMI ハードウェアに依存しない純粋なデータ型とロジック（設定パース、カーブ補間、PI 計算、IPC JSON プロトコル）。Linux 以外の開発マシン上でも単体テストが高速に実行可能。
- **`pmgfan_ipmi`**:
  ハードウェアとの対話を抽象化。`FanControlBackend` トレイトによって `ipmitool` コマンド経由と `/dev/ipmi0` ioctl 直接通信（Native）を透過的に切り替え可能。
- **`pmgfan_daemon`**:
  デーモンとしてのライフサイクル、systemd 連携、安全ステートマシン、非同期 IPC サーバーを担当。
- **`pmgfan_tui`**:
  クライアント側の CLI / TUI。ソケットを叩いて JSON-RPC 形式で応答を受け取り描画。

---

## 採用技術スタック

- **非同期ランタイム**: `tokio`
- **シリアライズ / デシリアライズ**: `serde`, `serde_json`, `toml`
- **CLI パース**: `clap`
- **ログ / トレーシング**: `tracing`, `tracing-journald`
- **TUI フレームワーク**: `ratatui`, `crossterm`
- **低レベルシステムコール**: `libc`, `nix`

---

## センサー収集メカニズム

### 1. ファン回転数（RPM）
iRMC S5 の SDR（Sensor Data Record）を IPMI 経由で取得。ファンステータス（OK / Disabled / Absent）と現在の回転数を 2000ms 周期でポーリング。

### 2. 温度データ（hwmon + IPMI）
温度取得のオーバーヘッドを抑えるため、毎回の外部コマンド spawn は行わず、`/sys/class/hwmon/` を走査して直接読み取る：
- `coretemp`（CPU パッケージ / 各コア温度）
- `pch_cannonlake` 等（チップセット PCH 温度）
- `nvme`（ストレージ温度）

hwmon で取得できないセンサーについては、IPMI SDR からの温度読み出しもフォールバックとして統合。
