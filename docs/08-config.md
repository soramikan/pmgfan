# 08. 設定ファイルリファレンス

パス: `/etc/pmgfand/config.toml`

リポジトリ内の雛形は [../config/pmgfand.toml](../config/pmgfand.toml)。

## 完全な例

```toml
[device]
model = "PRIMERGY TX1320 M4"
backend = "ipmitool"
interface = "open"
path = "/dev/ipmi0"

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
target = 2500

kp = 0.003
ki = 0.0001
deadband = 75

# min_pwm / max_pwm は未指定なら [control] の範囲を使う
# min_pwm = 10
# max_pwm = 100
```

## セクション

### `[device]`

| キー | 説明 |
|---|---|
| `model` | 期待する機種名。起動時に FRU Product Name と照合し、不一致なら起動しない |
| `backend` | `ipmitool`（既定・プロセス経由）/ `native` または `openipmi`（`/dev/ipmi0` を直接 ioctl、Phase 9）。CLI の `--backend` 指定が優先 |
| `interface` | ipmitool の `-I` 値。ローカルは `open`。CLI の `-I` 指定が優先。native では不使用 |
| `path` | native バックエンドの IPMI デバイスパス（既定 `/dev/ipmi0`）。CLI の `--device` 指定が優先 |

### `[monitor]`

| キー | 説明 |
|---|---|
| `fan_interval_ms` | IPMI SDR でファン RPM を読む周期 |
| `temperature_interval_ms` | hwmon 温度を読む周期 |
| `history_seconds` | TUI スパークライン用の保持期間 |

### `[control]`

| キー | 説明 |
|---|---|
| `mode` | 起動時モード: `auto`(=`irmc_auto`) / `fixed_pwm` / `curve` / `target_rpm`（要 `[target_rpm]`） |
| `fixed_pwm` | `mode = "fixed_pwm"` のときの PWM 値（必須） |
| `pwm_scope` | 強制 PWM の適用範囲。`all`（既定・全ファン）または `chassis`（FAN CPU/FANx SYS のみ強制し、PSU は iRMC 自動制御に残す）。PSU ファンは単独では強制できない（ファームウェア仕様） |
| `min_pwm` / `max_pwm` | PWM 許可範囲。min 未満は UI でも拒否。既定の下限は 30% だが config で 30 未満も設定可（警告が出る。実機では 0% でもシャーシファンはハードウェアフロアで回転継続する） |
| `step_up` / `step_down` | 1回の適用での PWM 変化上限 |
| `down_hysteresis` | 降圧方向のヒステリシス |
| `min_apply_interval_ms` | PWM 適用の最小間隔 |

### `[safety]`

| キー | 説明 |
|---|---|
| `sensor_stale_seconds` | 非空温度データの鮮度期限。超過で `fail_action` 発動 |
| `ipmi_failure_limit` | 読み取り/書き込み系それぞれの連続失敗回数の閾値（→ `Degraded`） |
| `cpu_emergency` / `pch_emergency` | 緊急温度。超過時はモードに関わらず 100% PWM（`Failsafe`） |
| `fail_action` | 監視系フェイル時の挙動。`irmc-auto`（既定・override 解除）または `full-speed`（100% 強制） |

### `[[curve]]`

| キー | 説明 |
|---|---|
| `sensor` | センサー名。論理エイリアス（`cpu_package`/`cpu`/`pch`/`ambient`/`nvme`）または `chip/label` 指定。解決規則は docs/03 参照 |
| `points` | `[temp, pwm]` または `{temp=..., pwm=...}` の配列。温度昇順・各値検証あり（不正なら起動時エラー） |

### `[target_rpm]`

| キー | 説明 |
|---|---|
| `reference_fan` | 目標 RPM の基準ファン（例: `FAN CPU`）。大小・前後空白は緩和して照合 |
| `target` | `mode = "target_rpm"` 起動時の目標 RPM（500..=20000） |
| `kp` / `ki` | PI ゲイン（0 以上・有限） |
| `deadband` | ±RPM 内なら PWM 不変 |
| `min_pwm` / `max_pwm` | PI 出力の制限。省略時は `[control]` の範囲 |

参照ファンが読めなくなるとデーモンは独自制御を捨てて
iRMC Auto へ退避する（`Degraded`）。モード突入時の初期 PWM は
`/var/lib/pmgfand/calibration.toml`（`pmgfanctl calibrate` の
計測結果）からの逆引きを優先し、なければ現在の適用 PWM から
開始する。
