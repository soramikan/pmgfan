# CLI リファレンス (`pmgfanctl`)

`pmgfanctl` は、`pmgfand` デーモンを非 root ユーザーからコマンドラインで操作するためのクライアントツールです。Unix ドメインソケット（既定: `/run/pmgfand/control.sock`）経由でデーモンと安全に通信します。

---

## コマンド一覧

| コマンド | 説明 | 主な用途 |
|---|---|---|
| `pmgfanctl` | TTY 接続時は TUI 起動、パイプ接続時は `status` 出力 | 手動での総合監視・操作 |
| `pmgfanctl tui` | TUI を明示的に起動 | ターミナル UI 画面を開く |
| `pmgfanctl status` | 現在のモード、ファン回転数、温度などを表示 | シェルスクリプト・監視ツール連携 |
| `pmgfanctl auto` | OEM 強制を解除し、iRMC 標準の自動制御へ戻す | 即座に標準制御へ戻したいとき |
| `pmgfanctl pwm <0-100>` | PWM 値を指定して固定回転（Fixed PWM）にする | 特定の回転数で検証したいとき |
| `pmgfanctl rpm <FAN> <RPM>` | 指定したファンを目標回転数（RPM）に維持する（PI 制御） | 静音と冷却のバランスを回転数で指定 |
| `pmgfanctl mode curve` | 設定ファイルのファンカーブ制御（Curve）に切り替える | 通常の自動制御へ戻すとき |
| `pmgfanctl scope <chassis\|all>` | PWM 強制の対象スコープを切り替える | 静音化（chassis）と全冷却（all）の切替 |
| `pmgfanctl calibrate` | PWM 10%〜100% の回転数を自動測定して保存する | Target RPM の精度向上 |

---

## 各サブコマンドの詳細と実行例

### 1. 状態表示: `pmgfanctl status`

現在のファン回転数、温度、動作モード、PWM 値、安全状態をプレーンテキストで表示します。

```bash
$ pmgfanctl status
Mode: Curve
PWM: 25% (Scope: Chassis)
Safety: Normal

Fans:
  FAN CPU   1450 RPM  [OK]
  FAN1 SYS  1280 RPM  [OK]
  FAN PSU1  1620 RPM  [OK] (iRMC Auto)
  FAN PSU2  1590 RPM  [OK] (iRMC Auto)

Temperatures:
  CPU Package   42.0 C
  PCH           54.0 C
  NVMe Drive    38.0 C
```

> [!TIP]
> パイプやリダイレクトで呼び出すと自動的に `status` モードで動作するため、`pmgfanctl | grep CPU` のような利用も可能です。

---

### 2. iRMC 標準制御へ復帰: `pmgfanctl auto`

pmgfan の独自制御および OEM PWM 強制を解除し、サーバーマザーボード（iRMC S5）本来の自動ファン制御へ戻します。

```bash
$ pmgfanctl auto
Mode switched to iRMC Auto (OEM override cleared).
```

---

### 3. PWM 値を固定: `pmgfanctl pwm <VAL>`

ファンを一定の PWM デューティ比（%）で固定します。

```bash
# シャーシファンを 25% で固定
$ pmgfanctl pwm 25
Mode switched to Fixed PWM: 25%
```

※ 設定ファイル `config.toml` の `min_pwm`（既定 10% など）未満、または `max_pwm`（既定 100%）を超える値はエラーになります。

---

### 4. 目標回転数で制御: `pmgfanctl rpm <FAN> <RPM>`

指定したファンの回転数を目標値に一致させる閉ループ PI 制御（Target RPM モード）を開始します。

```bash
# FAN CPU を 2000 RPM に維持
$ pmgfanctl rpm "FAN CPU" 2000
Mode switched to Target RPM: 2000 RPM (ref: FAN CPU)
```

- ファン名の空白や大小文字は柔軟に認識されます（例: `"FAN CPU"`, `"cpu"`, `"FAN1 SYS"`）。
- 参照ファンが読み取れなくなった場合は、安全のため自動的に `iRMC Auto` へ退避します。

---

### 5. ファンカーブ制御へ移行: `pmgfanctl mode curve`

`/etc/pmgfand/config.toml` で定義されたファンカーブに基づき、温度に応じた自動制御（Curve モード）に切り替えます。

```bash
$ pmgfanctl mode curve
Mode switched to Curve.
```

---

### 6. PWM スコープの変更: `pmgfanctl scope <chassis|all>`

デーモンを再起動することなく、PWM 強制の対象範囲を切り替えます。

```bash
# 静音運用: CPU / ケースファンのみ制御し、電源ファンは iRMC Auto に残す
$ pmgfanctl scope chassis
Scope switched to chassis.

# 全力冷却: 電源ファンも含めて強制制御する
$ pmgfanctl scope all
Scope switched to all.
```

---

### 7. 自動キャリブレーション: `pmgfanctl calibrate`

PWM を 10% 刻みで 100% まで段階的に変化させ、各 PWM 値に対するファンの実際の回転数を自動測定します。

```bash
$ pmgfanctl calibrate
Starting PWM-to-RPM calibration...
Calibrating 10%...
Calibrating 20%...
...
Calibration complete. Saved to /var/lib/pmgfand/calibration.toml
Restored previous mode: Curve
```

- 測定結果は `/var/lib/pmgfand/calibration.toml` に保存されます。
- キャリブレーションが完了していると、`pmgfanctl rpm` 実行時の初期 PWM 推定精度が大幅に向上し、目標回転数への到達が速くなります。
- 中断したいときは、別の端末から `pmgfanctl auto` などを実行すれば即座に安全に中断されます。

---

## 共通オプション

- `--socket <PATH>`: 通信先の Unix ドメインソケットパスを指定します（既定: `/run/pmgfand/control.sock`）。
- `-h, --help`: ヘルプを表示します。
- `-V, --version`: バージョンを表示します。
