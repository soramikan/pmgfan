# 05. Unix socket API

## ソケット

```text
/run/pmgfand/control.sock
```

権限:

```text
root:pmgfan
0660
```

ユーザーは `pmgfan` グループに追加して使う:

```bash
sudo usermod -aG pmgfan sora
```

外部ネットワークソケットは開かない。

## プロトコル

ローカル専用なので v1 は **JSON Lines** で十分。1行に1メッセージ。

### 状態取得

リクエスト:

```json
{"version":1,"type":"get_status"}
```

レスポンス:

```json
{
  "version": 1,
  "mode": "curve",
  "pwm": 42,
  "fans": [
    {"name":"FAN CPU","rpm":2875,"status":"ok"},
    {"name":"FAN1 SYS","rpm":2400,"status":"ok"}
  ],
  "temperatures": {
    "cpu": 44.0,
    "pch": 57.0
  }
}
```

### モード変更

Fixed PWM:

```json
{"type":"set_mode","mode":{"fixed_pwm":40}}
```

Target RPM:

```json
{"type":"set_mode","mode":{"target_rpm":{"fan":"FAN CPU","rpm":2500}}}
```

iRMC Auto:

```json
{"type":"set_mode","mode":"irmc_auto"}
```

## 権限モデル

- socket への接続 = 操作権限（読み取りも含めて `pmgfan` グループが必要）
- pmgfanctl 側に権限はなく、すべての操作は pmgfand 内で検証される
- 危険な操作（min_pwm 未満の PWM 指定など）はデーモン側で拒否する
