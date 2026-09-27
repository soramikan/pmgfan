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

### 実装済み（Phase 4 時点）

- **curve センサー全滅**: カーブ参照センサーが1つも解決
  できない tick で `clear_override` を実行し iRMC Auto へ退避
  （`state = Degraded`）
- **IPMI 連続失敗**: ファン読み取り・PWM 書き込みの連続失敗が
  `ipmi_failure_limit`（既定3）に達すると `state = Degraded`
- **モード変更の直列化**: `apply_mode` の iRMC Auto 即時解除と
  制御ループの PWM 書き込みは同一ミューテックスで直列化し、
  「Auto 表示なのに override が残る」レースを防ぐ
- **daemon shutdown**: SIGTERM/SIGINT で必ず `clear_override`
  を実行してから終了（実機検証済み）
- **不正設定**: `mode = "curve"` で `[[curve]]` 未定義、
  `mode = "fixed_pwm"` で `fixed_pwm` 未指定、昇順でない
  カーブ点などは起動時にエラー終了

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
