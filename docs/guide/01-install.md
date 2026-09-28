# インストールとセットアップガイド

Fujitsu PRIMERGY TX1320 M4 上で `pmgfan` を導入・稼働させる手順を解説します。

---

## 前提条件

- **対象ハードウェア**: Fujitsu PRIMERGY TX1320 M4（iRMC S5 搭載）
- **対象 OS**: Linux（RHEL / AlmaLinux / Rocky Linux 9〜10 等を推奨）
- **権限**: サービスの登録・起動には `root`（または `sudo`）権限が必要です。
- **IPMI インターフェース**:
  - カーネルモジュール `ipmi_devintf`, `ipmi_si` が読み込まれ、`/dev/ipmi0` が存在すること。
  - `backend = "ipmitool"`（既定）を利用する場合は `ipmitool` パッケージが必要です。
  - `backend = "native"` を利用する場合は `ipmitool` は不要ですが、`/dev/ipmi0` へのアクセスが必要です。

---

## 方法 1: RPM パッケージによる導入（推奨）

RHEL / AlmaLinux 系環境では、RPM パッケージをビルドしてインストールするのが最も簡単で安全です。

### 1. ビルド環境の準備とビルド

ビルド対象ホスト（または同等環境）で以下を実行します。

```bash
# ビルドに必要なツールをインストール
sudo dnf install -y rpm-build systemd-rpm-macros cargo git

# RPM のビルドスクリプトを実行
./packaging/build-rpm.sh
```

実行後、`~/rpmbuild/RPMS/<arch>/pmgfan-<version>-<release>.<arch>.rpm` にパッケージが生成されます。

### 2. パッケージのインストール

```bash
sudo dnf install -y ~/rpmbuild/RPMS/x86_64/pmgfan-*.rpm
```

RPM のインストールによって以下が自動的に配置・設定されます：
- `/usr/sbin/pmgfand`（デーモン実行ファイル）
- `/usr/bin/pmgfanctl`（CLI / TUI 実行ファイル）
- `/usr/lib/systemd/system/pmgfand.service`（systemd unit 定義）
- `/etc/pmgfand/config.toml`（設定ファイル、所有権 `root:pmgfan 0640`）
- システムグループ `pmgfan` の自動作成

### 3. 操作ユーザーを `pmgfan` グループに追加

`pmgfanctl`（CLI/TUI）は Unix ドメインソケット経由でデーモンと通信します。root 以外の一般ユーザーから操作できるように、ユーザーを `pmgfan` グループに追加します。

```bash
sudo usermod -aG pmgfan $USER
# グループ変更を反映するために再ログインするか、newgrp を実行します
newgrp pmgfan
```

### 4. サービスの起動と自動起動有効化

```bash
sudo systemctl enable --now pmgfand
```

---

## 方法 2: ソースコードからの手動ビルド・インストール

RPM を使わずに直接バイナリをビルド・配置することも可能です。

### 1. ビルド

```bash
cargo build --release
```

生成されるバイナリ:
- `target/release/pmgfand`
- `target/release/pmgfanctl`

### 2. バイナリとファイルの配置

```bash
# グループ作成
sudo groupadd -r pmgfan 2>/dev/null || true

# バイナリのインストール
sudo install -m 0755 target/release/pmgfand /usr/sbin/pmgfand
sudo install -m 0755 target/release/pmgfanctl /usr/bin/pmgfanctl

# 設定ファイルの配置
sudo mkdir -p /etc/pmgfand
if [ ! -f /etc/pmgfand/config.toml ]; then
    sudo install -m 0640 -g pmgfan config/pmgfand.toml /etc/pmgfand/config.toml
fi

# systemd サービスファイルの配置
sudo install -m 0644 systemd/pmgfand.service /usr/lib/systemd/system/pmgfand.service
sudo systemctl daemon-reload
```

### 3. グループ追加とサービス起動

```bash
sudo usermod -aG pmgfan $USER
sudo systemctl enable --now pmgfand
```

---

## 起動確認と初期テスト

### 1. systemd サービスの状態確認

```bash
systemctl status pmgfand
```

`Active: active (running)` になっており、watchdog が機能していることを確認します。

### 2. ログの確認

```bash
journalctl -u pmgfand -f
```

起動ログに以下のような出力がされていれば正常です：
- 機種検証（PRIMERGY TX1320 M4）に成功
- IPMI バックエンドの初期化完了
- センサー一覧の取得完了
- IPC ソケット `/run/pmgfand/control.sock` の待ち受け開始

### 3. CLI から状態を取得

```bash
pmgfanctl status
```

各ファンの回転数（RPM）や温度が表示されれば、正常にセットアップ完了です！

### 4. TUI の起動

```bash
pmgfanctl tui
# または単に
pmgfanctl
```

フルスクリーンのリアルタイム監視・制御画面が起動します（終了は `q`）。

---

## アンインストール

### RPM パッケージの場合

```bash
sudo dnf remove -y pmgfan
```

※ 編集済みの設定ファイル `/etc/pmgfand/config.toml` は `/etc/pmgfand/config.toml.rpmsave` として保持されます。

### 手動インストールの場合

```bash
sudo systemctl disable --now pmgfand
sudo rm -f /usr/lib/systemd/system/pmgfand.service
sudo systemctl daemon-reload
sudo rm -f /usr/sbin/pmgfand /usr/bin/pmgfanctl
# 必要に応じて設定ファイルとデータディレクトリを削除
# sudo rm -rf /etc/pmgfand /var/lib/pmgfand /run/pmgfand
```
