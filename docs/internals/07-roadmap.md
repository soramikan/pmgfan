# 07. 実装ロードマップと検証記録

## 実装フェーズの達成状況

本プロジェクトは以下の 9 つのフェーズに分けて段階的に開発・実機検証を進め、全フェーズの実装と実機検証が完了している。

| Phase | 機能・項目 | ステータス | 概要 |
|---|---|---|---|
| **Phase 1** | RPM/温度監視 + OEM PWM set/clear | ✅ 実機検証済み | SDR 取得、Fujitsu OEM コマンド疎通確認 |
| **Phase 2** | daemon + Unix socket | ✅ 実機検証済み | `pmgfand` 常駐化と JSON-RPC IPC ソケット |
| **Phase 3** | Fixed PWM / iRMC Auto | ✅ 実機検証済み | 手動固定 PWM および iRMC 自動制御への復帰 |
| **Phase 4** | ファンカーブ自動制御 | ✅ 実機検証済み | 温度に応じた線形補間、最大値調停 |
| **Phase 5** | セーフティ状態機械 + watchdog | ✅ 実機検証済み | systemd watchdog、緊急温度、センサー喪失・0 RPM 検知 |
| **Phase 6** | Ratatui TUI | ✅ 実機検証済み | リアルタイム監視、カーブグラフ、ダイアログ操作 |
| **Phase 7** | Target RPM PI コントローラ | ✅ 実機検証済み | 閉ループ PI 制御、不感帯、抗ワインドアップ |
| **Phase 8** | 自動キャリブレーション | ✅ 実機検証済み | 10% 刻み自動掃引、中央値計測、永続化 |
| **Phase 9** | native `/dev/ipmi0` バックエンド | ✅ 実機検証済み | OpenIPMI ioctl 直接通信、FRU/SDR 自前パース |

> [!NOTE]
> 安全機構（Phase 5）を先行して完成させてから Target RPM（Phase 7）を導入する順序を徹底した。これにより、センサー喪失時や通信途絶時にもファンが暴走することなく安全に iRMC Auto へ退避する堅牢性を確立した。

---

## 制御対象ハードウェアスコープ

PRIMERGY TX1320 M4（iRMC S5）における各ファンスロットの扱い：

- **制御対象**:
  - `FAN CPU`（CPU ファン）
  - `FAN1 SYS`（リアケースファン）
- **監視専用（既定 `pwm_scope = "chassis"` 時）**:
  - `FAN PSU1`（電源ユニット 1 ファン）
  - `FAN PSU2`（電源ユニット 2 ファン）
- **Disabled スロット**:
  - `FAN2 SYS` / `FAN PSU`（スロット検出のみ、非制御）

---

## 今後の拡張候補（検討中）

- Prometheus エクスポーター機能（メトリクス収集）
- 追加の Fujitsu PRIMERGY シリーズ（TX1330 M4 / RX シリーズなど）への対応検証
