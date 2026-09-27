# 09. 実装ロードマップ

| Phase | 実装 |
|---|---|
| 1 | RPM/温度監視 + OEM PWM set/clear |
| 2 | daemon + Unix socket |
| 3 | Fixed PWM / iRMC Auto |
| 4 | ファンカーブ |
| 5 | Safety state machine + systemd watchdog |
| 6 | Ratatui TUI |
| 7 | Target RPM PI controller |
| 8 | 自動キャリブレーション |
| 9 | native `/dev/ipmi0` backend |

**Phase 1〜5 を完成させてから Target RPM を入れる。**
安全機構なしに閉ループ制御を先に作ると、センサー喪失時に
暴走するリスクがあるため。

## 最終的な構成

```text
                    pmgfanctl
                  CLI / Ratatui TUI
                          │
                    Unix Socket
                          │
                          ▼
                     pmgfand
              ┌───────────┼───────────┐
              │           │           │
           Monitor      Control     Safety
              │           │           │
       hwmon + IPMI    Curve/PI    Watchdog
              │           │           │
              └───────┬───┴───────────┘
                      │
                Fujitsu backend
                      │
            ┌─────────┴────────┐
            │                  │
       IPMI SDR          OEM 0x2e/0xf5
       RPM/Temp             PWM
            │                  │
            └────────┬─────────┘
                     ▼
                  iRMC S5
```

## v1 の制御対象スコープ

- 制御対象: `FAN CPU`, `FAN1 SYS`（グローバル PWM 経由）
- 監視専用: `FAN PSU1`, `FAN PSU2`
- `FAN2 SYS` / `FAN PSU`（ Disabled スロット）: 検出のみ
