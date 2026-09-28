# 06. TUI 設計

`ratatui + crossterm` を使う。TUI はビューア/エディタであり、
閉じても pmgfand の制御は継続する。

実装状況: メイン画面（Mode/State/PWM・ファン一覧バー・
温度+スパークライン履歴・カーブプレビュー）、
`A`/`C`/`F` キーでのモード切替、Fixed PWM ダイアログ、
`E` カーブエディタ（`S` でデーモン検証+設定ファイル
永続化+即時適用）は実機動作確認済み。ログビューア・
Target RPM 画面は未実装（`L`/`R` は告知表示のみ）。
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
 [E] Edit curve [L] Logs   [Q] Quit
```

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

```text
Target RPM

 Reference fan:
 > FAN CPU

 Target:
   2500 RPM

 Current:
   2470 RPM

 PWM:
   39%

 Error:
   -30 RPM

 Controller:
   LOCKED
```

コントローラのステータス:

```text
SEARCHING
STABILIZING
LOCKED
LIMITED_MIN
LIMITED_MAX
SENSOR_LOST
```
