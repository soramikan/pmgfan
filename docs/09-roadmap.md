# 09. 実装ロードマップ

| Phase | 実装 | 状態 |
|---|---|---|
| 1 | RPM/温度監視 + OEM PWM set/clear | ✅ 実機検証済み |
| 2 | daemon + Unix socket | ✅ 実機検証済み |
| 3 | Fixed PWM / iRMC Auto | ✅ 実機検証済み |
| 4 | ファンカーブ | ✅ 実機検証済み |
| 5 | Safety state machine + systemd watchdog | 済み（watchdog/Degraded/緊急温度/センサー陳腐化/0 RPM/fail_action） |
| 6 | Ratatui TUI | 済み（監視・モード切替・Fixed/RPM ダイアログ・カーブエディタ・スコープ切替） |
| 7 | Target RPM PI controller | ✅ 実装済み（PI + デッドバンド + 抗ワインドアップ + 参照ファン喪失で Auto 退避） |
| 8 | 自動キャリブレーション | ✅ 実装済み（10%刻み掃引・中央値・`/var/lib/pmgfand/calibration.toml`・モード変更で中断・完了後に元モード復帰） |
| 9 | native `/dev/ipmi0` backend | ✅ 実機検証済み（ioctl トランスポート・Device ID・FRU・SDR 線形化・OEM 制御。`backend = "native"` で選択、既定は ipmitool） |

**Phase 1〜5 を完成させてから Target RPM を入れる。**
安全機構なしに閉ループ制御を先に作ると、センサー喪失時に
暴走するリスクがあるため（この順序を守った）。

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
