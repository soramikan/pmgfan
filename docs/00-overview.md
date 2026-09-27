# 00. 概要・目的・設計方針

## 目的

Fujitsu **PRIMERGY TX1320 M4**（iRMC S5）のファンを、iRMC 標準制御とは独立に
制御できるようにする。

- 静音化したい（深夜帯は低回転で運用したい）
- 逆に高負荷時は iRMC 標準より積極的に冷やしたい
- 温度・回転数を常時可視化したい
- 制御が壊れたときは**必ず iRMC 標準制御へ戻る**ことを保証したい

## 背景

既存実装 `fujitsu-1320-m4-fancontrol`（Python）が、TX1320 M4 / iRMC S5 に対して
Fujitsu OEM IPMI コマンドによる PWM 強制・解除を実証している。検証環境は
iRMC S5 3.31P / SDR 3.40。

本プロジェクトはこのプロトコル部分を Rust に移植し、その上に
安全機構・ファンカーブ・RPM フィードバック制御・TUI を載せる。

## 設計方針

### 権限分離

iRMC を直接触るのはデーモン `pmgfand` のみ。TUI/CLI `pmgfanctl` は
Unix socket 経由で指示を出すだけで、root 権限も `/dev/ipmi0` へのアクセスも
持たない。

### フェイルセーフ最優先

独自制御は「iRMC より賢い場合のみ」有効にする。判断に必要な情報
（温度・RPM・IPMI 応答）が欠けた瞬間に OEM override を解除し、
iRMC 標準制御へ戻す。詳細は [04-safety.md](04-safety.md)。

### Auto = iRMC 復帰

モード名 `Auto` は pmgfan 独自の自動制御**ではなく**、OEM 強制設定を解除して
iRMC 本来の制御へ戻すことを意味する。

### グローバル PWM のみを前提にする

実証済みの OEM コマンドは `0xff`（全 PWM チャンネル）への強制値設定。
ファン個別の独立制御は約束しない。PSU ファンは監視専用とする。
詳細は [03-control.md](03-control.md)。

### TUI を閉じても制御は継続

状態を持つのはデーモン。TUI はあくまでビューア/エディタであり、
閉じても systemd デーモンが制御を継続する。

## 名称

| 名前 | 種別 |
|---|---|
| `pmgfan` | プロジェクト名・リポジトリ名・制御グループ名 |
| `pmgfand` | デーモンバイナリ・systemd unit 名 |
| `pmgfanctl` | CLI + TUI バイナリ |

主要パス:

| パス | 用途 |
|---|---|
| `/etc/pmgfand/config.toml` | 設定ファイル |
| `/run/pmgfand/control.sock` | Unix socket |
| `/var/lib/pmgfand/calibration.toml` | キャリブレーション結果 |
| `pmgfan` グループ | pmgfanctl 利用を許可するグループ |
