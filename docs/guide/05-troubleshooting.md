# トラブルシューティングガイド

`pmgfan` の運用中によく発生する問題と、その原因および解決策をまとめました。

---

## よくある問題と解決策

### 1. `pmgfanctl` 実行時に `Permission denied` または接続エラーになる

#### 現象
```text
Error: Failed to connect to /run/pmgfand/control.sock: Permission denied (os error 13)
```

#### 原因
実行ユーザーが `pmgfan` システムグループに追加されていないか、追加後のグループ情報が現在のシェルセッションに反映されていません。

#### 解決策
1. ユーザーを `pmgfan` グループに追加します：
   ```bash
   sudo usermod -aG pmgfan $USER
   ```
2. 設定を現在のシェルに即時反映させるか、一度ログアウトして再ログインします：
   ```bash
   newgrp pmgfan
   ```
3. 所属グループを確認します：
   ```bash
   id
   # "pmgfan" が含まれていることを確認
   ```

---

### 2. デーモン（`pmgfand`）が起動に失敗する

#### 現象
`systemctl status pmgfand` を実行すると `Active: failed` になっている。

#### 原因と解決策

`journalctl -u pmgfand -e` を実行してエラーログを確認してください。

- **ケース A: `Device model mismatch` エラー**
  - **ログ**: `FRU product name does not match expected model 'PRIMERGY TX1320 M4'`
  - **原因**: 動作環境が TX1320 M4 でないか、FRU 情報が書き換わっています。
  - **対処**: 本ツールは PRIMERGY TX1320 M4（iRMC S5）専用です。他機種では動作保証されません。

- **ケース B: `/dev/ipmi0: No such file or directory` エラー**
  - **原因**: Linux カーネルの OpenIPMI ドライバモジュールがロードされていません。
  - **対処**: モジュールを手動ロードし、起動時自動ロードを設定します：
    ```bash
    sudo modprobe ipmi_si ipmi_devintf
    # 永続化
    echo -e "ipmi_si\nipmi_devintf" | sudo tee /etc/modules-load.d/ipmi.conf
    ```

- **ケース C: `ipmitool: command not found` エラー**
  - **原因**: `backend = "ipmitool"` が設定されていますが、システムに `ipmitool` がインストールされていません。
  - **対処**: `sudo dnf install -y ipmitool` でインストールするか、設定ファイルで `backend = "native"`（OpenIPMI 直接通信）に変更してください。

---

### 3. ファンが突然 100% の全開回転（爆音）になった

#### 現象
ファンが急激に最高速度で回転し始め、`pmgfanctl status` の Safety 欄が `Failsafe` または `Degraded` と表示される。

#### 原因
pmgfan の安全保護機能（フェイルセーフ）が発動しました。以下のいずれかが検知された可能性があります：
1. **緊急温度の検知**: CPU が 90°C、または PCH が 95°C を超過した。
2. **センサーデータの途絶**: hwmon の温度ファイルや IPMI からデータが 10 秒以上読めなくなった（`sensor_stale_seconds` 超過）。
3. **ファンの異常停止**: 0 RPM が検知された。

#### 解決策
1. `journalctl -u pmgfand -n 50` で発動原因ログを確認します。
2. ハードウェアのエアフロー、CPU ヒートシンクの装着、ホコリ詰まり等を確認します。
3. 温度が正常値に復帰し、センサーが安定すれば自動的に通常制御に戻ります。
4. 強制的に即座に安全復帰させたい場合は `pmgfanctl auto` を実行します。

---

### 4. 電源ファン（PSU ファン）が静かにならない

#### 現象
CPU ファンは静かになったが、電源ユニットのファンが高回転（~3700〜5600 RPM）で回り続けている。

#### 原因
強制 PWM の対象範囲が `pwm_scope = "all"` になっています。
TX1320 M4 の PSU ファンはハードウェア自律制御を持っており、`all` で PWM を強制すると逆に高速回転してしまう特性があります。

#### 解決策
PWM 強制スコープを `chassis` に変更します：
```bash
# 即座に反映
pmgfanctl scope chassis
```
設定ファイル `/etc/pmgfand/config.toml` 内の `pwm_scope = "chassis"` も確認してください。

---

### 5. Target RPM モードで回転数が合わない・ふらつく

#### 現象
`pmgfanctl rpm "FAN CPU" 2000` を設定したが、目標回転数に到達するまで時間がかかる、または目標値付近で安定しない。

#### 解決策
PWM と回転数の対応関係が未学習の状態です。自動キャリブレーションを実行してください：

```bash
pmgfanctl calibrate
```

約1〜2分で 10%〜100% の回転数特性が計測され、`/var/lib/pmgfand/calibration.toml` に保存されます。これにより初期 PWM 推定が極めて正確になります。

---

## 緊急時の手動リセット（完全復旧コマンド）

もし `pmgfand` プロセスがフリーズしたり、異常停止してファン制御が戻らなくなった場合は、以下の手動コマンドを実行して iRMC を本来の自動制御に戻すことができます。

### 方法 1: systemd サービスの停止（推奨）
デーモン停止時に自動でクリーンアップ（OEM 強制解除）が行われます：
```bash
sudo systemctl stop pmgfand
```

### 方法 2: ipmitool 生コマンドによる直接解除
デーモンが応答しない場合の最終手段です：
```bash
# Fujitsu OEM PWM 強制解除コマンド（NetFn=0x2e, Cmd=0xf5, 全解除 payload）
sudo ipmitool raw 0x2e 0xf5 0x80 0x28 0x00 0x2d 0x46 0x57 0x01 0xff 0x00 0x00
```
このコマンドを発行すると、iRMC S5 は即座にハードウェア本来の自動制御に戻ります。
