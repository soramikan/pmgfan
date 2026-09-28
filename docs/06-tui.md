# 06. TUI 設計

`ratatui + crossterm` を使う。TUI はビューア/エディタであり、
閉じても pmgfand の制御は継続する。

実装状況: メイン画面（Mode/State/PWM+スコープ・ファン一覧バー・
温度+スパークライン履歴・カーブプレビュー）、
`A`/`C`/`F`/`R` キーでのモード切替（`R` は Target RPM
ダイアログ: ↑↓ で参照ファン選択、←→ で目標 ±100）、
Fixed PWM ダイアログ、
`S` PWM スコープ切替（all ↔ chassis、設定ファイルへ永続化）、
`X` キャリブレーション開始（進行はヘッダーに `Calib n/m@p%`、
中断は任意のモード変更キー）、
`E` カーブエディタ（`S` でデーモン検証+設定ファイル
永続化+即時適用）は実装済み。ログビューアは未実装
（`L` は告知表示のみ）。
終了は `Q`/`Esc`/`Ctrl+C`。raw mode では Ctrl+C が
SIGINT にならないため、Ctrl 修飾付き文字キーはモード
操作として扱わない（誤操作防止）。ポーリング・モード
変更はバックグラウンドタスクで行い、daemon 停滞時も
UI は応答し続ける（要求タイムアウト 5s/10s）。
端末が小さすぎる場合は警告表示、カーブパネルは高さが
足りないとき自動で省略される。

## メイン画面

```text
 PRIMERGY TX1320 M4 Fan Control                         pm-01

 Mode: CURVE                     iRMC: OK
 PWM : 42%                       Controller: Healthy

 ┌ Fans ───────────────────────────────────────────────┐
 │ FAN CPU          2875 RPM    ██████████████    OK  │
 │ FAN1 SYS         2400 RPM    ███████████       OK  │
 │ FAN2 SYS              --     Disabled              │
 │ FAN PSU1         3760 RPM    █████████████████ OK  │
 │ FAN PSU2         3680 RPM    █████████████████ OK  │
 └─────────────────────────────────────────────────────┘

 ┌ Temperatures ───────────────────────────────────────┐
 │ CPU Package       43°C     ▁▂▂▃▃▄                  │
 │ PCH               57°C     ▃▃▄▅▅▅                  │
 │ NVMe              46°C     ▂▃▃▃▄                   │
 └─────────────────────────────────────────────────────┘

 ┌ Fan Curve ──────────────────────────────────────────┐
 │ 100% |                                      ●      │
 │  80% |                                ●            │
 │  60% |                          ●                  │
 │  40% |             ●────●                          │
 │  30% | ●────●                                      │
 │      +--------------------------------------------  │
 │       30  40  50  60  70  80  90°C                │
 └─────────────────────────────────────────────────────┘

 [A] iRMC Auto  [C] Curve  [F] Fixed PWM  [R] RPM
 [E] Edit curve [S] Scope  [X] Calibrate  [L] Logs   [Q] Quit
```

`S` キーで PWM スコープを `chassis` ↔ `all` にトグルする。
ヘッダーの PWM 表示の隣に `[chassis]` / `[all+PSU]` として
現行スコープを示す（`all` は PSU ファンも強制対象になる
ため黄色で注意表示）。切替は `set_pwm_scope` でデーモンへ
送られ、config ファイルにも永続化される。

## Curve エディタ（`E`）

```text
CPU Curve

 Temperature     PWM
 ──────────────────
 35°C             30%
 50°C             35%
 60°C             45%
 70°C             60%
 80°C             80%
 90°C            100%

 ↑↓ select
 ←→ PWM
 +/- Temperature
 A   add point
 D   delete
 S   save
 Esc cancel
```

保存前にデーモン側で検証する:

```text
温度は昇順か
PWMは0..100か
min_pwm以上か
最終点が十分高いか
```

## Fixed PWM（`F`）

```text
Fixed PWM

       ┌─────────────────────┐
       │        40 %         │
       └─────────────────────┘

 ←/→ ±1%
 Shift ←/→ ±5%

 Enter Apply
 Esc   Cancel
```

デフォルトでは `min_pwm = 30` 未満にはできない。
既存 TX1320 M4 向け実装も 30% 未満をデフォルト拒否している。

## Target RPM（`R`）

実装は簡易ダイアログ（ファン選択 + 目標値入力）:

```text
┌ Target RPM ──────────────────────┐
│  reference fan:                  │
│  > FAN CPU                       │
│    FAN1 SYS                      │
│                                  │
│  target: 2500 RPM                │
│  ↑↓ fan  ←→ ±100 (Shift ±500)    │
│  Enter apply  Esc cancel         │
└──────────────────────────────────┘
```

PI 制御自体はデーモン側（`[target_rpm]` の kp/ki/deadband）。
ヘッダーの Mode には `Target 2500 RPM (FAN CPU)` と出る。
参照ファンが読めなくなるとデーモンが iRMC Auto へ退避する
（`Degraded` 表示）。

## キャリブレーション（`X`）

`X` で PWM→RPM 自動計測を開始する（`start_calibration`）。
計測中はヘッダーに `Calib {step}/{total}@{pwm}%` が出る。
ファンが `min_pwm`..100% まで順に掃引されるため音が出る。
中断は `A`/`C`/`F`/`R` など任意のモード変更キーで行う
（完了時は開始前のモードへ自動復帰し、結果は
`/var/lib/pmgfand/calibration.toml` へ保存される）。
