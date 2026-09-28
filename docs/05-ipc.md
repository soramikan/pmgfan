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
  "pwm_scope": "chassis",
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

### カーブ取得・更新（TUI エディタ用）

```json
{"type":"get_curves"}
```

→ `{"type":"curves","curves":[{"sensor":"cpu_package","points":[[35,30],...]}]}`

```json
{"type":"set_curves","curves":[{"sensor":"cpu_package","points":[[35,30],[90,100]]}]}
```

`set_curves` はデーモン側で検証してから適用する
（2点以上・温度厳密昇順・PWM は `min_pwm..=max_pwm`・
温度 0..=150℃・最終点は `max_pwm` 以上 = 高温域で必ず
全開へ到達する）。`--config` で起動している場合は
`[[curve]]` セクションを設定ファイルへ書き戻してから
実行時状態へ適用する（永続化失敗時は適用しない）。
config 無し起動では実行時適用のみ。

### PWM スコープ切替

```json
{"type":"set_pwm_scope","scope":"chassis"}
```

`scope` は `"all"`（PSU を含む全ファン = OEM `0xff`）または
`"chassis"`（シャーシファンのみ = `0x03`、PSU は iRMC 自動制御）。
`set_curves` と同じく `--config` 起動時は `[control] pwm_scope`
を設定ファイルへ書き戻してから実行時状態へ適用する。
強制中に切り替わった場合、制御ループが次 tick で同じ PWM を
新スコープで書き直す（値が同じでもスコープバイトが異なる
別コマンドのため再送が必要）。

現在のスコープは `status` レスポンスの `pwm_scope` フィールドで
確認できる（ランタイム値が正。config は起動時の初期値）。

## 権限モデル

- socket への接続 = 操作権限（読み取りも含めて `pmgfan` グループが必要）
- pmgfanctl 側に権限はなく、すべての操作は pmgfand 内で検証される
- 危険な操作（min_pwm 未満の PWM 指定など）はデーモン側で拒否する

## 単一インスタンス

pmgfand は `/run/pmgfand/pmgfand.lock` を `flock(LOCK_EX|LOCK_NB)` で
排他取得する。2重起動は即座にエラー終了する。これにより、
旧プロセスの終了処理（socket 削除等）が新プロセスに干渉する
レースを防ぐ。
