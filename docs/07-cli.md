# 07. CLI 設計

TUI だけだと SSH スクリプト等から操作しにくいため、
`pmgfanctl` はサブコマンド方式の CLI も持つ。
いずれも Unix socket API（[05-ipc.md](05-ipc.md)）経由で pmgfand に指示する。

## コマンド

```bash
pmgfanctl                 # TTY なら TUI、パイプなら status
pmgfanctl tui             # TUI を明示起動
pmgfanctl status          # 状態表示
pmgfanctl auto            # iRMC Auto へ戻す
pmgfanctl pwm 40          # Fixed PWM 40%
pmgfanctl rpm "FAN CPU" 2500   # Target RPM（PI 制御）
pmgfanctl mode curve      # ファンカーブ制御
pmgfanctl scope chassis   # PWM 強制をシャーシファンのみに
pmgfanctl scope all       # PSU を含む全ファンに強制
pmgfanctl calibrate       # PWM→RPM 自動計測（中断は pmgfanctl auto 等）
```

TUI のカーブ表示は `--config <path>` で読んだ `[[curve]]`
定義を使う（既定 `/etc/pmgfand/config.toml`）。読めない場合は
カーブパネル自体を出さない。

## `status` 出力例

```text
Mode: Curve
PWM: 42%

FAN CPU   2875 RPM  OK
FAN1 SYS  2400 RPM  OK
FAN PSU1  3760 RPM  OK
FAN PSU2  3680 RPM  OK

CPU       43.0 C
PCH       57.0 C
```

systemd トラブル時の緊急操作としても使えるよう、出力は
パイプ・grep しやすいプレーンテキストとする（`--json` オプションも検討）。
