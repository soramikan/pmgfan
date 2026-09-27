# 09. 実装ロードマップ

| Phase | 実装 | 状態 |
|---|---|---|
| 1 | RPM/温度監視 + OEM PWM set/clear | ✅ 実機検証済み |
| 2 | daemon + Unix socket | ✅ 実機検証済み |
| 3 | Fixed PWM / iRMC Auto | ✅ 実機検証済み |
| 4 | ファンカーブ | ✅ 実機検証済み |
| 5 | Safety state machine + systemd watchdog | 済み（watchdog/Degraded/緊急温度/センサー陳腐化/0 RPM/fail_action） |
| 6 | Ratatui TUI | 済み（監視・モード切替・Fixed PWM ダイアログ。カーブエディタは今後） |
| 7 | Target RPM PI controller | 未着手 |
| 8 | 自動キャリブレーション | 未着手 |
| 9 | native `/dev/ipmi0` backend | 未着手 |

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
