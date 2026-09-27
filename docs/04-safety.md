# 04. フェイルセーフ・watchdog・systemd

このドキュメントは pmgfan で最優先の設計領域を扱う。

## 状態機械

```text
STARTING
   ↓
MONITORING
   ↓
CONTROLLING
   ↓
DEGRADED
   ↓
FAILSAFE
```

## FAILSAFE 条件

```text
必須温度センサーが10秒以上取得不能
RPM取得不能
ipmitool連続3回失敗
OEM command失敗
FAN CPU = 0 RPM
FAN1 SYS = 0 RPM
設定値異常
daemon shutdown
```

### 実装済み

- **curve センサー全滅**: カーブ参照センサーが1つも解決
  できない tick で `clear_override` を実行し iRMC Auto へ退避
  （`state = Degraded`）。解除失敗時は次 tick で再試行する
- **緊急温度**: `cpu_emergency` / `pch_emergency` 超過時は
  モードに関わらず 100% PWM を強制し `state = Failsafe` へ
  （docs/03 のカーブ誤設定に対する最後の砦）。温度が閾値を
  下回ると通常制御に復帰する。Auto モード中の発動では
  復帰時に `clear_override` して Monitoring に戻る
- **iRMC Auto の解除未達追跡**: Auto 起動時や `set_mode` の
  `clear_override` が失敗すると `clear_pending` を立て、
  mode が Auto の間制御ループが解除を再試行する
  （`state = Degraded` で未達を可視化。
  「Auto 表示だが強制が残っている」状態を放置しない）
- **IPMI 連続失敗**: 読み取り（fan/temp）・書き込み系の
  連続失敗がそれぞれ `ipmi_failure_limit`（既定3）に達すると
  `state = Degraded`。回復は全ドメインが健全になった時のみ
  （ドメイン間で状態が振動しない）
- **機種検証**: 起動時に FRU の Product Name を `device.model`
  と照合し、不一致・取得失敗では起動しない（fail-closed）
- **モード変更の直列化**: `apply_mode` の iRMC Auto 即時解除と
  制御ループの PWM 書き込みは同一ミューテックスで直列化し、
  「Auto 表示なのに override が残る」レースを防ぐ
- **daemon shutdown**: SIGTERM/SIGINT でラッチ付き通知
  （watch channel）を制御ループへ送り、in-flight の書き込み
  完了を待機（5s 超過時のみ abort）→ `clear_override` を実行
  してから終了（実機検証済み）
- **タスク死亡**: 監視・制御・IPC のいずれかが終了した場合、
  shutdown 処理のあと非0終了し `Restart=on-failure` に委ねる
- **ipmitool タイムアウト**: 各呼び出しは 15 秒で強制終了し、
  ハングした子プロセスがループを塞がないようにする
- **不正設定**: `mode = "curve"` で `[[curve]]` 未定義、
  `mode = "fixed_pwm"` で `fixed_pwm` 未指定/範囲外、
  `min_pwm < 30` や `min > max`、昇順でないカーブ点、
  空の `device.model`、非有限・範囲外の緊急温度などは
  起動時にエラー終了

未実装: `sensor_stale_seconds`（センサー陳腐化検出）、
`fail_action` 以外のフェイルアクション、0 RPM 検出
（いずれも Phase 5 予定）。

## FAILSAFE の動作

基本動作は **OEM PWM override の解除**（iRMC 標準制御へ戻す）。

ただし明白な過熱（例: `CPU >= cpu_emergency`）の場合のみ、
先に `100% PWM` を適用する。

```toml
[safety]
cpu_emergency = 90
pch_emergency = 95

sensor_stale_sec = 10
ipmi_failure_limit = 3

fail_action = "irmc-auto"
```

## デーモン終了時も必ず解除

SIGTERM:

```text
systemctl stop pmgfand
       ↓
OEM override clear
       ↓
iRMC Auto
       ↓
exit
```

## systemd watchdog

プロセスハングまで考慮して systemd watchdog を使う:

```ini
Type=notify
WatchdogSec=15s
```

デーモンから定期的に `WATCHDOG=1` を送る。推奨される通知間隔は
watchdog timeout のおおむね半分。

実装では、**ファンポーリングが直近 watchdog 周期内に成功して
いるときだけ**キックを送る。ポーリングがハングしている場合は
キックを止め、systemd がサービスを再起動して
`ExecStopPost` で iRMC Auto へ戻せるようにする。

## systemd unit

```ini
[Unit]
Description=pmgfan: PRIMERGY iRMC Fan Control Daemon
After=multi-user.target

[Service]
Type=notify
ExecStart=/usr/sbin/pmgfand \
    --config /etc/pmgfand/config.toml

ExecStopPost=/usr/sbin/pmgfand clear-override

Restart=on-failure
RestartSec=2s

WatchdogSec=15s
TimeoutStopSec=10s

RuntimeDirectory=pmgfand
RuntimeDirectoryMode=0750

ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
NoNewPrivileges=yes

[Install]
WantedBy=multi-user.target
```

`ExecStopPost` をデーモンとは別経路の直接解除にしておくのがポイント。
制御ループが壊れていても:

```text
systemd
 ↓
プロセス停止
 ↓
pmgfand clear-override
 ↓
iRMC Auto
```

へ戻せる。

リポジトリ内の雛形は [../systemd/pmgfand.service](../systemd/pmgfand.service)。
