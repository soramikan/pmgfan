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
poll_interval_ms = 2000

min_pwm = 30
max_pwm = 100

step_up = 20
step_down = 5

down_hysteresis = 5
min_change_interval_ms = 5000
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

## Target RPM（回転数指定制御）

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
