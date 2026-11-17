# B-413: Python の `WebSocketEndpoint` のクラスのルートは、app・router・ルートの依存（認証）を解かずに接続を受けていた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-17）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Class-based WebSocketEndpoint routes never run their declared dependencies」から確認。

## 事象

- `@app.websocket` に `veil.WebSocketEndpoint` のクラスを付けたルートは、Rust の `aio::websocket_endpoint` が
  アップグレードしてから `WebSocketRoute._endpoint_session` → `on_connect` を呼んでいた。`App(dependencies=...)`・
  `APIRouter(dependencies=...)`・`@app.websocket(dependencies=...)` から作った依存の木はどこでも解かれず、
  認証・認可の依存は黙って落ちていた（`async def` のハンドラは `_call_di` で解いている）。
- 認証の依存を app・router に置いたアプリでも、クラスのエンドポイントには認証無しで誰でも接続できた。

## 対応

- Rust の `aio::websocket_endpoint` を非同期にし、アップグレードの前に GIL のスレッド（`lane`）で
  `WebSocketRoute._endpoint_check(req)` を呼ぶ。依存を `_deps.solve_sync` で解き、`HTTPException` はその
  ステータス、引数の誤りは 422 でハンドシェイクを断る（`101` を返さない）。ほかの例外は 500。
- 通ったときは、依存に渡した `websocket`（依存が `websocket.state` に置いた値を持つ）と依存の文脈を
  `_endpoint_session` へ渡し、`yield` の依存の後ろは接続が閉じてから（`on_disconnect` の後）走らせる。
- 利用者ガイド（`library.md` の英日）と `WebSocketEndpoint` の docstring に書いた。

## テスト

- `crates/veil-python/tests/test_aio.py` の `test_endpoint_dependencies_run_before_upgrade`（app の認証の依存が
  403・422 で断る、router の `yield` の依存は閉じた後に終わる、`websocket.state` が引き継がれる）。
