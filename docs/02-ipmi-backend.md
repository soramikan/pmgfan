# 02. IPMI 層・Fujitsu OEM コマンド

## v1 は ipmitool プロセス経由

最初から独自の OpenIPMI ioctl 実装は書かない。v1 では:

```text
Rust
  ↓
Command::new("ipmitool")
  ↓
ipmitool -I open
  ↓
/dev/ipmi0
```

`ipmitool` はローカル OpenIPMI インターフェースを正式にサポートしており、
`-c` で `sdr` / `sensor` の CSV 出力も利用できる。
**人間向けテキストを正規表現解析するより CSV を使う方が堅牢。**

RPM 取得例:

```bash
ipmitool -I open -c sdr type fan
```

内部モデル:

```rust
struct FanReading {
    id: FanId,
    name: String,
    rpm: Option<u32>,
    status: SensorStatus,
    updated_at: Instant,
}
```

TX1320 M4 の SDR 例:

```text
FAN CPU       2875 RPM
FAN1 SYS      2400 RPM
FAN2 SYS      Disabled
FAN PSU       Disabled
FAN PSU1      3760 RPM
FAN PSU2      3680 RPM
```

## Fujitsu OEM 制御

既存実装と互換のプロトコルを使う。

| 項目 | 値 |
|---|---|
| Fujitsu IANA | `00 28 80` |
| OEM コマンド内表現（little endian） | `80 28 00` |
| NetFn | `0x2e` |
| Command | `0xf5` |

PWM 強制 payload（例: 40% → `0x28`）:

```text
80 28 00
2d 46 57 01
<scope> 80 <PWM>
```

`<scope>` は強制の適用範囲:

| 値 | 意味 |
|---|---|
| `0xff` | 全ファン（PSU 電源ファンを含む） |
| `0x03` | シャーシファン（FAN CPU / FANx SYS）のみ。PSU は iRMC 自動制御に残る |

実機観測（iRMC S5 / TX1320 M4、スコープ値の全走査済み）:
`W` が受理するのは `0x03` と `0xff` のみ。`0x00〜0x32` の他の値、
count+エントリ形式、`idx|0x80` 形式、フラグバイトの下位ビット
によるスロット選択、`-F` 以外のタグ — いずれも受理されない
（`0xc7`/`0xc9`）。**PSU ファンを単独で強制する手段は無い**。

また PSU ファンは Entity 10.x（Power Supply ドメイン）に属し、
PSU 内蔵の自律制御を持つ。`0xff` で強制 duty を書いても PSU は
自前のフロアを優先する（全体 10% 強制でも ~3760RPM を維持し、
Auto の ~1600RPM より遅くならない）。つまり PSU 側を Auto に
残す `pwm_scope = "chassis"` が静音上も最良の設定である。

注意: シャーシファンを強く絞ると PSU 吸気温度が上がり、
PSU ファンが自律的に増速する（シャーシ冷却との連動）。

さらに実機観測: `W` 書き込みはスコープ内のファンにだけ
作用し、**スコープ外のファンの強制状態は変化しない**
（ラッチされる）。`0xff` で PSU を強制した後に `0x03` で
シャーシだけ書き直しても、PSU は旧強制値のまま残る。
切替時に PSU を解放するには先に `ff 00 00` のクリアが必須。
pmgfand はスコープ変更を検知した tick で clear → 新スコープ
での再書き込みを行い、起動時にも一度だけ `clear_override`
で iRMC 側の残存強制を正規化する。

解除 payload（常に全スコープ）:

```text
80 28 00
2d 46 57 01
ff 00 00
```

### force スロット読み出し（実機観測）

`R`(0x52) タグで読み出せる。要求 payload:

```text
80 28 00 | 2d 46 52 01 | <count> | [idx 00]...
```

応答（実機観測）:

```text
80 28 00 | 01 | <count> | (index|flags, value)×count
```

- index バイト: 下位6bit がスロット index、bit7 が強制フラグ
- value バイト: 強制時は PWM%（例: 40% 強制中 `c0 28`、自動時 `40 59`）

スロット index は 0..=31。`irmc_fan.py` の既定値は `0x00, 0x01, 0x19, 0x1a`。
`pmgfand read` で確認できる。

## Backend 抽象化

OEM 制御を trait で隔離する（実装は `crates/ipmi/src/backend.rs`）:

```rust
trait FanControlBackend {
    fn model_name(&self) -> impl Future<Output = Result<String>> + Send;
    fn fans(&self) -> impl Future<Output = Result<Vec<FanReading>>> + Send;
    fn temperatures(&self) -> impl Future<Output = Result<Vec<TempReading>>> + Send;
    fn set_pwm(&self, scope: PwmScope, pwm: u8) -> impl Future<Output = Result<()>> + Send;
    fn clear_override(&self) -> impl Future<Output = Result<()>> + Send;
    fn read_override_slots(&self, indices: &[u8])
        -> impl Future<Output = Result<Vec<PwmSlot>>> + Send;
}
```

`Backend` enum が実行時に実装を切り替える:

```text
Backend::Ipmitool   既定。ipmitool プロセス経由（-I open）
Backend::Native     /dev/ipmi0 を直接 ioctl（Phase 9）
MockBackend         テスト用
```

## native `/dev/ipmi0` バックエンド（Phase 9）

`backend = "native"`（または `openipmi`）で ipmitool を介さず
カーネルの OpenIPMI デバイスを直接叩く。

```text
pmgfand
  ↓ ioctl
/dev/ipmi0          IPMICTL_SEND_COMMAND / poll / IPMICTL_RECEIVE_MSG
  ↓
iRMC S5 (KCS システムインターフェース)
```

実装: `crates/ipmi/src/native.rs`。

- **トランスポート**: `IPMICTL_SEND_COMMAND` で `ipmi_system_interface_addr`
  （addr_type `0x0c`, channel `0x0f`）宛に送信し、`poll` + `IPMICTL_RECEIVE_MSG`
  で msgid/netfn/cmd を照合して応答を受け取る。
  **受信時は `addr_len` と `msg.data_len` にバッファ容量を設定して渡すこと**
  （0 のまま渡すと `EMSGSIZE` になる）。デバイスは `O_NONBLOCK` で開き、
  `poll` が `POLLERR`/`POLLHUP`/`POLLNVAL` を返したら即エラー。
  それでも `EMSGSIZE` が来た場合（応答が受信バッファより大きい）は
  `IPMICTL_RECEIVE_MSG_TRUNC` で残りを読み捨ててキューを回復させる
  — 置き去りにすると以後の全要求が別メッセージの応答で汚染される。
- **機種検証**: `Get Device ID`（netfn 0x06 cmd 0x01）で Manufacturer/Product ID を
  取得し、FRU を `Get FRU Inventory Area Info`（0x10）で列挙 →
  common header → Product Info Area を読み Product Name を抽出する。
  TX1320 M4 では Product Name は FRU 2（Chassis）にある。
  FRU 読み出し（0x11）は count バイトが u8 なのでチャンクを 255 以下に抑える。
- **SDR**: `Reserve SDR Repository`（0x22）→ `Get SDR`（0x23）で全レコードを
  列挙。ヘッダ5バイトを読んでから `rec[4]` の長さ分だけ継続読み出しする
  （固定サイズで一括読みすると末尾超過で失敗する BMC がある）。
  `Get SDR` の offset は u8 なので 255 を超える位置は要求できない
  — offset=255 では残量全てを一度に要求し、レコードは仕様上限の
  260B（5+255）まで読める。予約喪失（0xc5）は再予約して再試行。
  レコード ID の再訪・上限超過（1024）はエラーにし、BMC が `next_id`
  を誤って返してもデバイスロックを握ったまま無限巡回しない。
  パース結果は TTL（300s）付きでキャッシュし、ポーリング毎の
  全レコード再走査を避ける（読み取りエラー時も invalidate）。
- **センサー変換**: Full Sensor Record（type 0x01）のみパースし、
  `Get Sensor Reading`（0x2d）の生値を `y = (M·x + B·10^Bexp) · 10^Rexp`
  で線形化。ファンは sensor type 0x04、温度は 0x01（unit が °C=0x01
  以外の温度センサーは除外）。応答 flags byte（Table 35-15）は
  bit7=events enabled / bit6=scanning enabled / bit5=reading
  unavailable。scanning disabled（bit6=0）はラッチ済みの古い値なので
  欠測扱い。非線形（linearization ≠ 0）も欠測扱い。
  `Get Sensor Reading` が返す completion code で欠測に写すのは
  実機確認済みの `0xcb`（sensor 不在）と `0xcd`（領域不在）だけ
  — それ以外（0xc0 busy / 0xc1 invalid / 0xc3 timeout / 0xd4 privilege /
  0xff 等）は全て `Err` で伝播する。同様にトランスポート層エラー
  （IO・タイムアウト・形式不正）も `Disabled` に潰さず `Err` —
  デーモンの `ipmi_failure_limit` フェイルセーフが正しく
  カウントできるようにするため。
- **OEM 制御**: `fujitsu::*_data()` のペイロードをそのまま
  netfn 0x2e / cmd 0xf5 に流す。ipmitool backend と完全に共通。

実機検証（pm-01）:

| 項目 | 結果 |
|---|---|
| `probe` | Device ID / FRU Product Name `PRIMERGY TX1320 M4` 取得成功 |
| `fans` | `ipmitool sdr type fan` と全スロット一致 |
| `temps` | `ipmitool sdr type Temperature` と全センサー一致 |
| `read` / `set-pwm` / `clear-override` | OEM 強制・解除ともに動作 |
| `run` | 機種検証・ソケット・Curve 制御・SIGTERM 時 override 解除を確認 |

制約:

- `/dev/ipmi0` が必要（OpenIPMI ドライバ + `ipmi_devintf`）。root か
  デバイスへの rw 権限が要る。`interface` 設定は native では無意味。
- LAN 経由の IPMI（`lanplus` 等）には対応しない — リモート運用は
  `ipmitool` backend を使う。

## 起動時検証

既存実装の検証範囲は TX1320 M4 / iRMC S5 3.31P / SDR 3.40 であり、
他ファームウェアは保証されていない。pmgfand は起動時に機種名・
iRMC バージョンを確認し、想定外であれば制御を開始しない
（警告して MONITORING に留まるか、fail-closed で終了するかは設定で選択）。
