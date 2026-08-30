# B-84: HTTP/3 のストリーミングバックエンド経路が `use_h2c` を無視する

## 事象

HTTP/3 のバックエンド中継には 2 経路ある。

- **非ストリーミング**: `http3_server.rs::handle_request_impl` の
  `let use_h2c = target.use_h2c || upstream_group.use_h2c();` 分岐（B-39/B-74）で
  `proxy_to_h2c_backend_async` を選ぶ。**h2c 対応済み。**
- **ストリーミング**: `Decision::Stream(BackendTaskParams { .. })`
  （`http3_server.rs` 1228 行付近）→ `http3_stream::run_backend_task`。
  **`BackendTaskParams` に `use_h2c` フィールドが存在せず**、`use_tls` / `sni` /
  `tls_insecure` しか持たない。`build_h1_request_head` で組んだ **HTTP/1.1** を
  そのまま送る。

このため **HTTP/3 クライアント → h2c 上流**の構成は、ストリーミング経路に入ると
必ず失敗する。相手が h2c 専用サーバ（veil の `h2c_listen` 等）だと

- バックエンド側: `[H2C Worker] Plain HTTP/1.1 not supported on H2C-only server, closing connection`
- veil 側: `[HTTP/3] streaming backend read error: Connection reset by peer (os error 104)` → **502**

## 再現

`use_h2c = true` の上流プールを参照するルートへ **HTTP/3** でリクエストする
（HTTP/1.1・HTTP/2 クライアントからは同じルートが 200 で通る）。
F-170 の E2E（`/uds-h2c/` を HTTP/3 クライアントで叩く）で再現した。

## 影響

**gRPC over HTTP/3 の中継が壊れる**（gRPC 上流は h2c 必須）。
`tools/perf` の `grpc_h3*` 構成が F-167 まで「クライアント非対応で NA」だったため、
この経路は長らく計測もテストもされていなかった。

## 改修案

`BackendTaskParams` に `use_h2c: bool` を追加し、`run_backend_task` で
`proxy_to_h2c_backend_async` 相当（`H2C_POOL` + `h3_h2c_connect_and_handshake`）へ
分岐させる。ストリーミング（全二重）と HTTP/2 ストリームの対応付けが要るため、
`http3_stream.rs` の `RespMsg` チャネル設計をそのまま使えるかの検討が必要。

暫定回避としては、h2c 上流を使うルートでストリーミング経路に入らないよう
バッファリング設定を `full` にする方法がある。

## 発見

F-170（UDS バックエンド）の E2E 追加時。UDS とは無関係で、TCP の h2c 上流でも同じ。
