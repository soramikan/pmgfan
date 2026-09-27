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

親ディレクトリ `/run/pmgfand` は `root:pmgfan 0750` が必要
（socket へ辿り着くための traversal 権限）。systemd では
unit の `RuntimeDirectoryMode=0750` + `Group=pmgfan` で用意し、
手動起動では pmgfand が自ら作成した場合に同じ権限を付与する
（既存ディレクトリの権限は変更しない）。

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

レスポンス（Phase 2 実装。`state`/`uptime_secs` を含む）:

```json
{
  "version": 1,
  "type": "status",
  "state": "controlling",
  "mode": {"fixed_pwm": 40},
  "pwm": 40,
  "fans": [
    {"name":"FAN CPU","rpm":2875,"status":"ok"},
    {"name":"FAN1 SYS","rpm":2400,"status":"ok"}
  ],
  "temperatures": [
    {"chip":"ipmi","label":"CPU","celsius":36.0},
    {"chip":"coretemp","label":"Package id 0","celsius":37.0}
  ],
  "uptime_secs": 71.2
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

ファンカーブ（Phase 4 実装。`[[curve]]` 未設定時はエラー）:

```json
{"type":"set_mode","mode":"curve"}
```

iRMC Auto:

```json
{"type":"set_mode","mode":"irmc_auto"}
```

`target_rpm` は Phase 7 予定のため現在は
`{"type":"error","error":"mode not implemented yet (roadmap phase 7)"}`
を返す。

## 権限モデル

- socket への接続 = 操作権限（読み取りも含めて `pmgfan` グループが必要）
- pmgfanctl 側に権限はなく、すべての操作は pmgfand 内で検証される
- 危険な操作（min_pwm 未満の PWM 指定など）はデーモン側で拒否する

## 単一インスタンス

pmgfand は `/run/pmgfand/pmgfand.lock` を `flock(LOCK_EX|LOCK_NB)` で
排他取得する。2重起動は即座にエラー終了する。これにより、
旧プロセスの終了処理（socket 削除等）が新プロセスに干渉する
レースを防ぐ。
