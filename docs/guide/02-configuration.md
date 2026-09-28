# 設定リファレンスと静音化ガイド

`/etc/pmgfand/config.toml` の全設定項目と、PRIMERGY TX1320 M4 で安全に静音運用するための実践的な設定ノウハウを解説します。

リポジトリ内のサンプルファイル: [`config/pmgfand.toml`](../../config/pmgfand.toml)

---

## 設定ファイル全体例

```toml
[device]
model = "PRIMERGY TX1320 M4"
# backend: "ipmitool"（既定。ipmitool プロセス経由）または
#          "native" / "openipmi"（/dev/ipmi0 を直接 ioctl、ipmitool 不要）
backend = "ipmitool"
interface = "open"
# native バックエンド用デバイスパス（backend = "native" 時のみ使用）
path = "/dev/ipmi0"

[monitor]
fan_interval_ms = 2000
temperature_interval_ms = 1000
history_seconds = 900

[control]
mode = "curve"

# 強制 PWM の適用範囲:
#   "chassis": FAN CPU / FANx SYS のみ。PSU ファンは iRMC 自動制御に残す（推奨・静音向け）
#   "all":     全ファン（PSU 電源ファンを含む）
pwm_scope = "chassis"

# 強制 PWM の許可範囲（%）
min_pwm = 10
max_pwm = 100

step_up = 20
step_down = 5
down_hysteresis = 5
min_apply_interval_ms = 5000

[safety]
sensor_stale_seconds = 10
ipmi_failure_limit = 3

# 緊急温度（°C）。超過時は全モードを中断して 100% PWM（Failsafe）
cpu_emergency = 90
pch_emergency = 95

# 監視失敗時の動作: "irmc-auto"（iRMC 制御へ戻す・推奨）または "full-speed"（100% 強制）
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
```

---

## 設定項目リファレンス

### `[device]`（ハードウェア接続）

| キー | 型 | 既定値 | 説明 |
|---|---|---|---|
| `model` | 文字列 | `"PRIMERGY TX1320 M4"` | 期待する機種名。起動時に FRU（Chassis）の Product Name と照合し、不一致なら制御を開始しません。 |
| `backend` | 文字列 | `"ipmitool"` | IPMI 通信方式。`"ipmitool"`（ipmitool コマンド経由）または `"native"` / `"openipmi"`（`/dev/ipmi0` direct ioctl）。 |
| `interface` | 文字列 | `"open"` | `backend = "ipmitool"` 実行時に渡す `-I` オプション値。通常は `"open"`。 |
| `path` | 文字列 | `"/dev/ipmi0"` | `backend = "native"` で利用する IPMI キャラクタデバイスのパス。 |

### `[monitor]`（監視インターバル）

| キー | 型 | 既定値 | 説明 |
|---|---|---|---|
| `fan_interval_ms` | 整数 | `2000` | IPMI SDR からファン回転数（RPM）を取得する周期（ミリ秒）。 |
| `temperature_interval_ms` | 整数 | `1000` | Linux hwmon / IPMI から温度を取得する周期（ミリ秒）。 |
| `history_seconds` | 整数 | `900` | TUI 用の履歴保持期間（秒）。 |

### `[control]`（ファン制御パラメータ）

| キー | 型 | 既定値 | 説明 |
|---|---|---|---|
| `mode` | 文字列 | `"curve"` | デーモン起動時の初期制御モード。`"auto"` / `"fixed_pwm"` / `"curve"` / `"target_rpm"`。 |
| `fixed_pwm` | 整数 | なし | `mode = "fixed_pwm"` の場合に必須の初期 PWM 値（0〜100）。 |
| `pwm_scope` | 文字列 | `"chassis"` | 強制 PWM の適用範囲。`"chassis"`（CPU/ケースファンのみ）または `"all"`（PSU ファン含む）。**静音運用には `"chassis"` を強く推奨**。 |
| `min_pwm` | 整数 | `10` | 許可する最小 PWM（%）。TUI や CLI からもこの下限を下回る値は設定できません。 |
| `max_pwm` | 整数 | `100` | 許可する最大 PWM（%）。通常は 100。 |
| `step_up` | 整数 | `20` | 1回の制御サイクルで許容する PWM の最大上昇幅（急激な回転数上昇を抑制）。 |
| `step_down` | 整数 | `5` | 1回の制御サイクルで許容する PWM の最大下降幅（ハンチング・急激な減速を防止）。 |
| `down_hysteresis` | 整数 | `5` | ファンを下げる方向に要求された際、この値以上の差がないと降圧を保留します。 |
| `min_apply_interval_ms` | 整数 | `5000` | PWM 書き込みを行う最小間隔（ミリ秒）。頻繁な書き込みによるファンの唸りを防止。 |

### `[safety]`（安全保護・フェイルセーフ）

| キー | 型 | 既定値 | 説明 |
|---|---|---|---|
| `sensor_stale_seconds` | 整数 | `10` | 温度データが更新されないままこの秒数が経過すると、センサー喪失とみなしてフェイルセーフを発動します。 |
| `ipmi_failure_limit` | 整数 | `3` | IPMI 読み書きが連続で失敗した回数の上限。超過すると `Degraded` 状態へ移行します。 |
| `cpu_emergency` | 整数 | `90` | CPU 緊急温度（°C）。到達すると即座に 100% PWM（`Failsafe`）を発動します。 |
| `pch_emergency` | 整数 | `95` | PCH 緊急温度（°C）。到達すると即座に 100% PWM（`Failsafe`）を発動します。 |
| `fail_action` | 文字列 | `"irmc-auto"` | 異常発生時の対処。`"irmc-auto"`（OEM override を解除し、iRMC 標準制御へ委ねる・推奨）または `"full-speed"`（PWM 100% 強制）。 |

### `[[curve]]`（温度連動ファンカーブ）

複数のカーブを定義できます（CPU と PCH など）。複数カーブがある場合、**最も高い PWM を要求しているカーブの値が採用**されます（最大値選択）。

- `sensor`: センサーのエイリアス（`cpu_package`, `cpu`, `pch`, `ambient`, `nvme`）または hwmon チップ名/ラベル。
- `points`: `[温度(°C), PWM(%)]` のペアの配列。温度の昇順で定義します。
  - 例: `[35, 30]` は 35°C 以下で 30% PWM。
  - 点の間は線形補間されます。

### `[target_rpm]`（目標回転数 PI 制御）

`mode = "target_rpm"` 時に指定した目標 RPM に追従させる PI コントローラの設定です。

- `reference_fan`: 基準とするファン名（例: `"FAN CPU"`）。
- `target`: 目標 RPM（500〜20000）。
- `kp`: 比例ゲイン（推奨: `0.003`）。誤差 100 RPM あたり PWM を約 0.3% 修正。
- `ki`: 積分ゲイン（推奨: `0.0001`）。定常偏差をゆっくりゼロに収束させる。
- `deadband`: 不感帯（RPM）。目標との差がこの範囲内なら PWM を変更しません（推奨: `75`）。

---

## TX1320 M4 静音化の実践ノウハウ

### 1. PSU ファンの特性と `pwm_scope = "chassis"`

PRIMERGY TX1320 M4 の電源ユニット（PSU）内蔵ファンには独自の特性があります：

- **PSU 内蔵ファンは自律制御**:
  PSU ファンはファームウェア（iRMC）とは独立した自律フロアを持っています。
- **`pwm_scope = "all"` の落とし穴**:
  全ファン強制（`scope = all`）で PWM 30% を指定すると、PSU ファンは約 **5600 RPM** という高回転で回ってしまい、非常に大きな風切り音が発生します。
- **Auto 時の PSU ファン**:
  iRMC の標準自動制御下では、低負荷時に PSU ファンは約 **1600 RPM** 前後まで静かに下がります。
- **対策**:
  **`pwm_scope = "chassis"`** を設定することで、CPU ファンおよびリアファン（FAN1 SYS）のみを pmgfan の制御下に置き、PSU ファンは iRMC の静かな自動制御に任せることができます。

> [!TIP]
> 静音化を目的として `pmgfan` を運用する場合は、必ず `pwm_scope = "chassis"` に設定してください。

### 2. シャーシファンのハードウェアフロア

TX1320 M4 のシャーシファン（FAN CPU / FAN1 SYS）は、PWM を 0% や 10% に下げても完全に停止することはありません。ハードウェア的な回転数フロア（約 775 RPM）が存在し、最低限の気流が保たれます。

そのため、アイドル時や軽負荷時は `min_pwm = 10` や `points = [[35, 20], ...]` などの低い値を設定しても安全に最低回転数を維持し、非常に静音な運用が可能です。

### 3. おすすめの静音ファンカーブ設定

家庭内サーバーや夜間運用に適したおすすめのカーブ設定です：

```toml
[[curve]]
sensor = "cpu_package"
points = [
    [40, 15],  # 40°C 以下: 最低限の静音回転（約800〜1000 RPM）
    [55, 25],  # 55°C 付近: 常用域。静音を維持
    [65, 40],  # 65°C: 負荷上昇に伴い冷却強化
    [75, 65],  # 75°C: しっかり冷やす
    [85, 90],  # 85°C: 高負荷時
    [90, 100], # 90°C: 緊急冷却
]

[[curve]]
sensor = "pch"
points = [
    [50, 20],
    [65, 30],
    [75, 50],
    [85, 80],
    [95, 100],
]
```

### 4. 設定の反映手順

設定ファイルを編集した後は、デーモンを再起動して設定を再読み込みします。

```bash
sudo systemctl restart pmgfand
```

再起動後、状態を確認します：
```bash
pmgfanctl status
```
