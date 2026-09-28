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

OEM 制御を trait で隔離する:

```rust
trait FanControlBackend {
    async fn fans(&self) -> Result<Vec<FanReading>>;
    async fn set_global_pwm(&self, pwm: u8) -> Result<()>;
    async fn clear_override(&self) -> Result<()>;
    async fn read_override_slots(&self) -> Result<Vec<PwmSlot>>;
}
```

これにより実装を交換可能にする:

```text
IpmitoolBackend   v1 標準。ipmitool プロセス経由
OpenIpmiBackend   将来。/dev/ipmi0 を直接 ioctl
MockBackend       テスト用
```

## 起動時検証

既存実装の検証範囲は TX1320 M4 / iRMC S5 3.31P / SDR 3.40 であり、
他ファームウェアは保証されていない。pmgfand は起動時に機種名・
iRMC バージョンを確認し、想定外であれば制御を開始しない
（警告して MONITORING に留まるか、fail-closed で終了するかは設定で選択）。
