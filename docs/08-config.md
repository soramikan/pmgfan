# 08. 設定ファイルリファレンス

パス: `/etc/pmgfand/config.toml`

リポジトリ内の雛形は [../config/pmgfand.toml](../config/pmgfand.toml)。

## 完全な例

```toml
[device]
model = "PRIMERGY TX1320 M4"
backend = "ipmitool"
interface = "open"

[monitor]
fan_interval_ms = 2000
temperature_interval_ms = 1000
history_seconds = 900

[control]
mode = "curve"

min_pwm = 30
max_pwm = 100

step_up = 20
step_down = 5
down_hysteresis = 5
min_apply_interval_ms = 5000

[safety]
sensor_stale_seconds = 10
ipmi_failure_limit = 3

cpu_emergency = 90
pch_emergency = 95

fail_action = "irmc-auto"

[[curve]]
sensor = "cpu_package"

points = [
    [35, 30],
    [50, 35],
    [60, 45],
    [70, 60],
    [80, 80],
    [90, 100],
]

[[curve]]
sensor = "pch"

points = [
    [45, 30],
    [60, 35],
    [70, 45],
    [80, 65],
    [90, 100],
]

[target_rpm]
reference_fan = "FAN CPU"

kp = 0.003
ki = 0.0001
deadband = 75

min_pwm = 30
max_pwm = 100
```

## セクション

### `[device]`

| キー | 説明 |
|---|---|
| `model` | 期待する機種名。起動時検証に使う |
| `backend` | `ipmitool`（v1）。将来 `openipmi` |
| `interface` | ipmitool の `-I` 値。ローカルは `open` |

### `[monitor]`

| キー | 説明 |
|---|---|
| `fan_interval_ms` | IPMI SDR でファン RPM を読む周期 |
| `temperature_interval_ms` | hwmon 温度を読む周期 |
| `history_seconds` | TUI スパークライン用の保持期間 |

### `[control]`

| キー | 説明 |
|---|---|
| `mode` | 起動時モード: `auto`(=`irmc_auto`) / `fixed_pwm` / `curve` / `target_rpm`（未実装・起動時拒否） |
| `fixed_pwm` | `mode = "fixed_pwm"` のときの PWM 値（必須） |
| `min_pwm` / `max_pwm` | PWM 許可範囲。min 未満は UI でも拒否 |
| `step_up` / `step_down` | 1回の適用での PWM 変化上限 |
| `down_hysteresis` | 降圧方向のヒステリシス |
| `min_apply_interval_ms` | PWM 適用の最小間隔 |

### `[safety]`

| キー | 説明 |
|---|---|
| `sensor_stale_seconds` | この秒数センサー値が更新されなければ FAILSAFE |
| `ipmi_failure_limit` | 連続失敗回数の閾値 |
| `cpu_emergency` / `pch_emergency` | 緊急温度。超過時は 100% PWM |
| `fail_action` | `irmc-auto`（override 解除） |

### `[[curve]]`

| キー | 説明 |
|---|---|
| `sensor` | センサー名。論理エイリアス（`cpu_package`/`cpu`/`pch`/`ambient`/`nvme`）または `chip/label` 指定。解決規則は docs/03 参照 |
| `points` | `[temp, pwm]` または `{temp=..., pwm=...}` の配列。温度昇順・各値検証あり（不正なら起動時エラー） |

### `[target_rpm]`

| キー | 説明 |
|---|---|
| `reference_fan` | 目標 RPM の基準ファン（例: `FAN CPU`） |
| `kp` / `ki` | PI ゲイン |
| `deadband` | ±RPM 内なら PWM 不変 |
| `min_pwm` / `max_pwm` | PI 出力の制限 |
