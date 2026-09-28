# 03. IPMI 層・Fujitsu OEM コマンド仕様

## 概要

PRIMERGY TX1320 M4（iRMC S5）のファン制御プロトコルと、Rust における 2 つのバックエンド実装（`ipmitool` 経由 / `native` OpenIPMI ioctl 直接通信）の詳細仕様を解説する。

---

## Fujitsu OEM ファン制御プロトコル

iRMC S5 に対するファン PWM 強制および解除は、以下の Fujitsu OEM IPMI コマンドによって行われる。

| 項目 | 値 |
|---|---|
| Fujitsu IANA Enterprise Number | `00 28 80` |
| ペイロード内 OEM IANA 表現（リトルエンディアン） | `80 28 00` |
| ネットワークファンクション (NetFn) | `0x2e` (OEM / Group Extension) |
| コマンド (Cmd) | `0xf5` |

### 1. PWM 強制書き込み (`W` コマンド)

PWM デューティ比を任意の値（例: 40% → `0x28`）に強制設定する。

```text
要求ペイロード:
80 28 00 2d 46 57 01 <scope> 80 <PWM>
```

- `2d 46 57 01`: Fujitsu OEM ヘッダ (`-FW\x01`)
- `<scope>`: 強制を適用する対象範囲
  - `0x03`: **シャーシファンのみ**（FAN CPU / FAN1 SYS）。PSU 電源ファンは iRMC 自動制御に残る。
  - `0xff`: **全ファン**（PSU 電源ファンを含む全スロット）。
- `80`: 固定フラグ
- `<PWM>`: 目標デューティ比（`0x00`〜`0x64`, 0%〜100%）

### 2. PWM 強制解除 (`ff 00 00`)

OEM 強制設定を解除し、マザーボード（iRMC S5）本来の自動ファン制御へ復帰させる。

```text
要求ペイロード:
80 28 00 2d 46 57 01 ff 00 00
```

このコマンドが発行されると、iRMC S5 のハードウェアロジックが直ちにファン制御の主導権を取り戻す。

---

## PSU 電源ファン制御の実機調査結果

実機（TX1320 M4 / iRMC S5 3.31P）における徹底的なコマンド走査および挙動調査から、以下の重要な事実が判明している：

1. **PSU ファンを単独で制御するコマンドは存在しない**:
   - スコープバイトとして受理されるのは `0x03` と `0xff` のみ。
   - `0x00〜0x32`、スロット指定、ビットマスク指定などはいずれも iRMC によりエラー（`0xc7` / `0xc9`）として拒否される。
2. **PSU ファンの自律制御フロア**:
   - PSU ファンは IPMI 仕様上の Entity 10.x（Power Supply）に属し、電源ユニット内部のマイコンによる自律制御を持っている。
   - 全体強制 `0xff` で低いデューティ比（例: 10% や 30%）を書き込んでも、PSU は自前のフロアを優先し、約 3760〜5600 RPM という極めて高い回転数を維持してしまう。
   - 一方、iRMC の標準自動制御下（オーバーライド解除状態）では、低負荷時に PSU ファンは約 1600 RPM まで静かに減速する。
3. **スコープ切替時のラッチ挙動**:
   - `0xff`（全ファン）で強制した後に `0x03`（シャーシのみ）へ書き直しても、PSU ファン側には前回の強制値がラッチされて残り続ける。
   - そのため、スコープを切り替える際には **必ず一度 `ff 00 00` で全解除を行ってから** 新しいスコープで書き直す必要がある（`pmgfand` はこれを自動で行う）。

以上の理由から、本ツールでは **`pwm_scope = "chassis"`（スコープ `0x03`）を静音化における標準設定** としている。

---

## force スロット状態の読み出し (`R` コマンド)

各ファンスロットのオーバーライド状態は `R`(0x52) コマンドで問い合わせ可能。

```text
要求ペイロード:
80 28 00 2d 46 52 01 <count> [idx 00]...
```

応答ペイロード:
```text
80 28 00 01 <count> (index|flags, value)×count
```
- 下位 6 bit: スロット index
- bit 7: 強制中フラグ
- value: 設定されている PWM%

---

## バックエンドの抽象化 (`FanControlBackend`)

ハードウェア対話は `crates/ipmi/src/backend.rs` の非同期トレイトとして抽象化されている。

```rust
pub trait FanControlBackend: Send + Sync {
    fn model_name(&self) -> BoxFuture<'_, Result<String>>;
    fn fans(&self) -> BoxFuture<'_, Result<Vec<FanReading>>>;
    fn temperatures(&self) -> BoxFuture<'_, Result<Vec<TempReading>>>;
    fn set_pwm(&self, scope: PwmScope, pwm: u8) -> BoxFuture<'_, Result<()>>;
    fn clear_override(&self) -> BoxFuture<'_, Result<()>>;
    fn read_override_slots(&self, indices: &[u8]) -> BoxFuture<'_, Result<Vec<PwmSlot>>>;
}
```

---

## Native バックエンド (`crates/ipmi/src/native.rs`)

`backend = "native"` 設定時、外部の `ipmitool` プロセスを fork/exec せず、Linux カーネルの OpenIPMI キャラクタデバイス（`/dev/ipmi0`）と ioctl で直接通信する。

### 1. 通信メカニズム
- `/dev/ipmi0` を `O_NONBLOCK` でオープン。
- `IPMICTL_SEND_COMMAND` ioctl により、System Interface アドレス（addr_type `0x0c`, channel `0x0f`）宛に IPMI リクエストを送信。
- `poll()` システムコールで応答を監視し、`IPMICTL_RECEIVE_MSG` ioctl でレスポンスを受信。
- 受信バッファ超過（`EMSGSIZE`）発生時は、キュー詰まりを防止するため `IPMICTL_RECEIVE_MSG_TRUNC` で残余メッセージを安全にドレイン。

### 2. 機種・FRU の自動検証
- 起動時に `Get Device ID`（NetFn `0x06`, Cmd `0x01`）で BMC の基本情報を取得。
- `Get FRU Inventory Area Info`（NetFn `0x0a`, Cmd `0x10`）および `Read FRU Data`（Cmd `0x11`）により、Chassis / Board / Product Info Area を走査。
- Product Name が `"PRIMERGY TX1320 M4"` に一致することを確認。

### 3. SDR（Sensor Data Record）の列挙と線形化
- `Reserve SDR Repository`（Cmd `0x22`）および `Get SDR`（Cmd `0x23`）で全センサーレコードを走査。
- レコードは 300 秒間キャッシュされ、毎回の不要な SDR 走査を防止。
- `Get Sensor Reading`（NetFn `0x04`, Cmd `0x2d`）で得た生データ（8bit）を、SDR 内の変換係数（M, B, K1, K2）を用いて以下の公式で物理量に線形化：
  $$y = (M \cdot x + B \cdot 10^{B_{exp}}) \cdot 10^{R_{exp}}$$
- センサーの scanning disabled フラグや非線形データは厳密に判定し、異常時は安全にエラー伝播する。
