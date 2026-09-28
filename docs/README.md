# pmgfan ドキュメントポータル

Fujitsu PRIMERGY TX1320 M4（iRMC S5）専用ファンコントロールツール群 `pmgfan` の総合ドキュメントです。

目的や用途に合わせて、以下の各ガイドおよび仕様書を参照してください。

---

## 📖 利用者向けガイド (`docs/guide/`)

日常的な導入・設定・運用に関するドキュメントです。

| ドキュメント | 内容 |
|---|---|
| [**01. インストールとセットアップ**](guide/01-install.md) | RPM パッケージ導入、ソースビルド、systemd 設定、ユーザーグループ権限付与 |
| [**02. 設定リファレンスと静音化ガイド**](guide/02-configuration.md) | `config.toml` 全設定項目、静音化の鍵となる `pwm_scope = "chassis"`、推奨ファンカーブ例 |
| [**03. CLI リファレンス (`pmgfanctl`)**](guide/03-cli.md) | 全サブコマンド（`status`, `auto`, `pwm`, `rpm`, `mode`, `scope`, `calibrate`）の実行例と解説 |
| [**04. TUI 操作ガイド (`pmgfanctl tui`)**](guide/04-tui.md) | ターミナル UI の画面レイアウト、キーバインド一覧、ダイアログ操作方法 |
| [**05. トラブルシューティング**](guide/05-troubleshooting.md) | 権限エラー、起動失敗、爆音発生時のフェイルセーフ対応、緊急リセット手順 |

---

## 🛠 内部設計・開発者向け仕様書 (`docs/internals/`)

システムのアーキテクチャ、制御ロジック、ハードウェアプロトコルに関する技術仕様書です。

| ドキュメント | 内容 |
|---|---|
| [**01. 概要・目的・設計方針**](internals/01-overview.md) | プロジェクトの目的、背景、root 権限分離とフェイルセーフの設計思想 |
| [**02. システムアーキテクチャ**](internals/02-architecture.md) | 全体構成図、Rust workspace（クレート分割）、hwmon / IPMI センサー収集 |
| [**03. IPMI 層・Fujitsu OEM 仕様**](internals/03-ipmi.md) | NetFn `0x2e` / Cmd `0xf5` プロトコル、PSU ファンの挙動調査、OpenIPMI ioctl 実装 |
| [**04. ファン制御アルゴリズム**](internals/04-control.md) | レート制限・ヒステリシス、カーブ補間、Target RPM PI 制御、自動キャリブレーション |
| [**05. 安全機構・フェイルセーフ仕様**](internals/05-safety.md) | セーフティ状態機械（Normal / Degraded / Failsafe）、Watchdog 連携、クリーンアップ |
| [**06. Unix Socket IPC 仕様**](internals/06-ipc.md) | JSON-RPC 形式のソケットプロトコル、リクエスト/レスポンス型定義 |
| [**07. 実装ロードマップと検証記録**](internals/07-roadmap.md) | Phase 1〜9 の実装達成状況、実機検証記録、対象ハードウェアスコープ |
