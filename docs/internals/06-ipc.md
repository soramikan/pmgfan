# 06. Unix Socket IPC プロトコル仕様

## 概要

`pmgfand`（デーモン）と `pmgfanctl`（CLI / TUI）間の通信は、ローカル Unix ドメインソケット上で動作する JSON-RPC 形式の軽量プロトコルによって行われる。

- **ソケットパス**: `/run/pmgfand/control.sock`
- **パーミッション**: `root:pmgfan 0750`（`RuntimeDirectory=pmgfand` により systemd が作成）
- **メッセージ形式**: 1行1メッセージの改行区切り JSON（JSON Lines / newline-delimited JSON）

実装コード:
- 定義: [`crates/core/src/protocol.rs`](../../crates/core/src/protocol.rs)
- デーモン側サーバー: [`crates/daemon/src/ipc.rs`](../../crates/daemon/src/ipc.rs)

---

## リクエスト (`IpcRequest`)

クライアントからデーモンへ送信するメッセージ形式。

```json
{"type": "<RequestType>", ...}
```

### 1. `GetStatus`（現在の状態を取得）

```json
{"type": "GetStatus"}
```

### 2. `SetMode`（制御モードの変更）

```json
// iRMC Auto へ戻す
{"type": "SetMode", "mode": {"type": "Auto"}}

// Fixed PWM 40% に設定
{"type": "SetMode", "mode": {"type": "FixedPwm", "pwm": 40}}

// ファンカーブ制御へ移行
{"type": "SetMode", "mode": {"type": "Curve"}}

// Target RPM 制御（FAN CPU を 2500 RPM に維持）
{"type": "SetMode", "mode": {"type": "TargetRpm", "fan": "FAN CPU", "target": 2500}}
```

### 3. `SetScope`（PWM 強制スコープの変更）

```json
// シャーシファンのみ強制（PSU は Auto）
{"type": "SetScope", "scope": "chassis"}

// 全ファン強制
{"type": "SetScope", "scope": "all"}
```

### 4. `StartCalibration`（自動計測の開始）

```json
{"type": "StartCalibration"}
```

---

## レスポンス (`IpcResponse`)

デーモンからクライアントへ返される応答メッセージ形式。

### 1. `Ok`（コマンド成功）

```json
{"type": "Ok"}
```

### 2. `Error`（エラー発生）

```json
{"type": "Error", "message": "PWM value 120 is out of allowed range (10..=100)"}
```

### 3. `Status`（状態応答）

`GetStatus` リクエストに対する応答。

```json
{
  "type": "Status",
  "status": {
    "mode": {
      "type": "Curve"
    },
    "current_pwm": 35,
    "scope": "chassis",
    "safety": "Normal",
    "fans": [
      {
        "id": "FAN CPU",
        "name": "FAN CPU",
        "rpm": 1450,
        "status": "Ok"
      },
      {
        "id": "FAN1 SYS",
        "name": "FAN1 SYS",
        "rpm": 1280,
        "status": "Ok"
      },
      {
        "id": "FAN PSU1",
        "name": "FAN PSU1",
        "rpm": 1620,
        "status": "Ok"
      },
      {
        "id": "FAN PSU2",
        "name": "FAN PSU2",
        "rpm": 1590,
        "status": "Ok"
      }
    ],
    "temperatures": [
      {
        "name": "cpu_package",
        "temp_c": 42.0
      },
      {
        "name": "pch",
        "temp_c": 54.0
      }
    ],
    "timestamp": 1727521200
  }
}
```

---

## エラーハンドリングとタイムアウト

- クライアント側（`pmgfanctl`）は、ソケット接続およびレスポンス待機に 3 秒のタイムアウトを設定している。
- デーモン側がビジーまたは応答しない場合、クライアントは適切なエラーメッセージを出力して終了する。
