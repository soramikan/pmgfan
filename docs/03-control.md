# 03. 制御設計

## 制御モード

デーモンは4モードを持つ。

| Mode | 動作 |
|---|---|
| `Auto` | OEM override を解除し、完全に iRMC へ返す |
| `FixedPwm` | 例: PWM 40% 固定 |
| `Curve` | 温度から PWM を計算 |
| `TargetRpm` | RPM フィードバックから PWM を調整 |

重要:

```text
Auto ≠ pmgfan 独自ファンカーブ
Auto = OEM 強制設定を解除して iRMC 本来の制御へ戻す
```

## ファンカーブ

設定はセンサーごとに持つ。

CPU:

```toml
[[curve]]
sensor = "cpu_package"

points = [
    { temp = 35, pwm = 30 },
    { temp = 50, pwm = 35 },
    { temp = 60, pwm = 45 },
    { temp = 70, pwm = 60 },
    { temp = 80, pwm = 80 },
    { temp = 90, pwm = 100 },
]
```

PCH:

```toml
[[curve]]
sensor = "pch"

points = [
    { temp = 45, pwm = 30 },
    { temp = 60, pwm = 35 },
    { temp = 70, pwm = 45 },
    { temp = 80, pwm = 65 },
    { temp = 90, pwm = 100 },
]
```

NVMe も同様に追加できる。

### 要求値の合成

各センサーのカーブを線形補間で評価し、**最大値を採用**する:

```text
CPU要求 = 42%
PCH要求 = 35%
NVMe要求 = 55%

        ↓

最終PWM = max(42, 35, 55)
        = 55%
```

サーバ冷却では CPU だけを見ると PCIe/NVMe/PCH が高温でもファンを
下げてしまうため、最大値合成が必須。

## ヒステリシス

単純な即時反映だと:

```text
60.0°C → 45%
59.9°C → 35%
60.0°C → 45%
```

のように回転数が頻繁に変動する。冷却方向は即応、静音方向は慎重にする:

```toml
[control]
min_pwm = 30
max_pwm = 100

step_up = 20
step_down = 5

down_hysteresis = 5
min_apply_interval_ms = 5000
```

挙動イメージ:

```text
温度上昇
40 → 60 → 80%
       ↑速い

温度下降
80 → 75 → 70 → 65...
       ↑ゆっくり
```

### 実装（Phase 3/4、`crates/core/src/control.rs`）

- 書き込み周期は `min_apply_interval_ms` の tick で、1 tick の
  変化量を `step_up`/`step_down` で制限する
- **下降ヒステリシス**: 現在値と目標の差が `down_hysteresis`
  未満の間は下降を開始しない。一度下降を開始したら
  （差 ≥ hysteresis）、deadband を突き抜けて目標到達まで
  `step_down` ずつ継続する
- 目標値は事前に `min_pwm`..=`max_pwm` にクランプする

## 制御ループの構造（Phase 3/4 実装）

PWM への書き込みはデーモンの **制御ループのみ** が行う
（`crates/daemon/src/daemon.rs` の `control_loop`）。

```text
apply tick (min_apply_interval_ms)
   │
   ├─ IrmcAuto   → 何もしない（解除済み）
   ├─ FixedPwm   → cur != p なら即書込み。値が同じでも
   │               REASSERT_TICKS(6) ごとに再送
   │               （外部要因で override が消えた場合に備える）
   └─ Curve      → curve_demand() = 解決可能な全カーブの
                   要求値の最大 → RateLimiter → set_global_pwm
                   全センサー解決不能 → clear_override +
                   state=Degraded（iRMC Auto へ退避）
```

- **モード変更**: `mode_generation` カウンタをインクリメントし、
  制御ループが変化を検知して RateLimiter をリセットする
- **直列化**: 制御ループの PWM 操作と `apply_mode` の
  `IrmcAuto` 即時解除は同一ミューテックス（`ctrl`）を取る。
  「モード表示は Auto なのに override が残る」レースを防ぐ
- `set_mode` の `fixed_pwm`/`curve` は共有状態の更新のみで、
  実際の書き込みは次 tick で制御ループが行う。
  `irmc_auto` は残留 override が危険なため即時解除する

### センサー名の解決（`crates/core/src/sensor.rs`）

カーブの `sensor` は論理名で、実センサーはエイリアスから解決する
（大文字小文字・`_input` 接尾辞は正規化）:

| 論理名 | 解決順 |
|---|---|
| `cpu_package` | `coretemp` / `Package id 0` → `ipmi` / `CPU` |
| `cpu` | `ipmi` / `CPU` → `coretemp` / `Package id 0` |
| `pch` | `ipmi` / `PCH` → `pch_cannonlake` / `temp1` |
| `ambient` | `ipmi` / `Ambient` 系 |
| `nvme` | `nvme` hwmon チップ |

エイリアスに一致しない場合は `chip/label` 形式や
ラベル名での直接指定も試す。

## Target RPM（回転数指定制御・Phase 7 予定）

実証済みの OEM コマンドは `set RPM` ではなく `set PWM`。
そのため目標回転数は閉ループで実現する:

```text
Target RPM = 2500
       ↓
現在RPM = 2200
       ↓
PWMを上げる
       ↓
RPM = 2440
       ↓
少し上げる
       ↓
RPM = 2510
```

### PI 制御

ファンでは D 項はほぼ不要なので PI で十分:

```text
error = target_rpm - current_rpm

pwm += Kp × error
     + Ki × integral(error)
```

設定例:

```toml
[target_rpm]
reference_fan = "FAN CPU"
target = 2500

deadband_rpm = 75

kp = 0.003
ki = 0.0001

min_pwm = 30
max_pwm = 100
```

`±deadband_rpm` 以内では PWM を変更しない。

## 制約: ファン個別制御はしない

実証済みなのは `0xff`（全 PWM チャンネル）への強制値設定のみ。
v1 では:

```text
FAN CPU  = 2500 RPM
FAN1 SYS = 1500 RPM
```

のような完全独立制御を約束しない。`TargetRpm` は:

```text
reference_fan = FAN CPU
target = 2500 RPM
       ↓
PWM global = 43%
       ↓
FAN CPU  → 約2500 RPM
FAN1 SYS → そのPWMに対応するRPM
```

となる。**PSU ファンは監視専用**で制御対象から除外する。

将来、PWM slot と物理ファンの対応が実機で確定した段階で
`set_pwm(channel, pwm)` を追加できるよう、backend trait は
`read_override_slots()` を用意している。

## RPM キャリブレーション

TUI から PWM→RPM 特性を自動計測する:

```text
PWM 30%
↓
10秒待つ
↓
RPM中央値取得

PWM 35%
↓
10秒待つ
↓
RPM中央値取得

...
```

結果イメージ:

```text
Calibration
30%   → 1710 RPM
35%   → 2050 RPM
40%   → 2450 RPM
45%   → 2840 RPM
50%   → 3220 RPM
...
```

保存先:

```text
/var/lib/pmgfand/calibration.toml
```

`Target 2500 RPM` 指定時に最初から約 40% に飛べるため、
PI 制御の収束が大幅に速くなる。
