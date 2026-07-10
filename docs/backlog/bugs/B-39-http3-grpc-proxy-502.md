# B-39: HTTP/3 上の gRPC プロキシが 502 Bad Gateway を返す

## 事象

E2E `test_grpc_over_http3`（F-91）が失敗する。

- 経路: HTTP/3 POST `/grpc.test.v1.TestService/UnaryCall`、`Content-Type: application/grpc`、正規 length-prefix フレーム
- 期待: HTTP 200 + `grpc-status` トレーラーまたは gRPC ボディ
- 実際: HTTP **502**、`content-type: text/plain`、`server: veil/http3`、body_len=11（`Bad Gateway`）

同一環境の gRPC over TLS HTTP/1.1→H2C 経路（既存 `test_grpc_unary_call` 等）は成功する。

### 再現手順

1. `./tests/e2e_setup.sh start`
2. `cargo test --test e2e_tests --features full test_grpc_over_http3 -- --exact --nocapture`

### ログ証拠

```
[HTTP/3] Async backend proxy error: Invalid HTTP response
```

gRPC ルートは `use_h2c = true` の `grpc-pool` を指す。HTTP/3 プロキシ経路が **HTTP/1.1 で H2C 上流に接続**し、応答を HTTP/1.1 としてパースして失敗している可能性が高い。

## 影響

- `grpc-full` feature が謳う **HTTP/3 上 gRPC** が実運用不能
- ブラウザ外クライアントの H3+gRPC 利用が不可

## 調査メモ

- `src/http3_server.rs`: `is_grpc_request` は検出するが、ルーティング後は通常の `handle_proxy` / streaming バッファ経路へ
- `send_grpc_response` はルート未ヒット時のローカルエラー応答向け
- HTTP/2 クライアント経路は `use_h2c` で H2C 上流クライアントを使うが、HTTP/3→上流は H1 head 構築（`build_h1_request_head`）中心

## 改修案

1. HTTP/3 の gRPC リクエストで `use_h2c` 上流へ **H2C クライアント**を使う（既存 HTTP/2 gRPC プロキシ経路を再利用）
2. 上流の DATA + trailers を H3 レスポンス + trailers に変換
3. `test_grpc_over_http3` を 200 + grpc-status で回帰固定
4. 可能なら streaming gRPC over H3 も追加

## 関連

- F-91 / `docs/artifacts/test_coverage_report.md` E-G-06
- feature `grpc-full = ["grpc", "grpc-web", "http3"]`

## 対応状況

**完了**（2026-07-10）

- `src/http3_server.rs`: `use_h2c` 時に `proxy_to_h2c_backend_async`（H2C Prior Knowledge + `H2cClient`）
- gRPC はフルパス保持（`/*` プレフィックス除去で UNIMPLEMENTED になる問題を回避）
- trailers（`grpc-status` 等）をクライアントへ転送
- 回帰: `test_grpc_over_http3` PASS（HTTP 200 + grpc-status）
