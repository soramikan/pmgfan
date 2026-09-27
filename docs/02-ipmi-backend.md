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
ff 80 <PWM>
```

解除 payload:

```text
80 28 00
2d 46 57 01
ff 00 00
```

`0xff` は**全 PWM チャンネルへの強制値**を意味する。個別チャンネル制御は
実機で slot と物理ファンの対応が確定してから扱う
（[03-control.md](03-control.md) の制約を参照）。

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
