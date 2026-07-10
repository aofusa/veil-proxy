# B-40: gRPC プロキシがルート `/*` プレフィックスを除去し UNIMPLEMENTED / 502 になる

## 事象

F-92 の詳細 E2E（`test_grpc_http2_framing` / `test_grpc_streaming_detailed` / `test_grpc_trailer_detailed`）が HTTP **502** または
既存 `test_grpc_unary_call` が **HTTP 200 + grpc-status: 12 (UNIMPLEMENTED)** を返す。

- 経路: TLS HTTP/1.1 または HTTP/2 → プロキシ → H2C 上流 `grpc-pool`（127.0.0.1:9004）
- ルート: `path = "/grpc.test.v1.TestService/*"`, `use_h2c = true`
- クライアント path: `/grpc.test.v1.TestService/UnaryCall`
- 上流へ送られていた path: **`/UnaryCall`**（サービス名が落ちている）
- tonic 上流はメソッド未実装として `grpc-status: 12` を返す。H2C 失敗時は 502。

HTTP/3 側は B-39 でフルパス保持済み。H1/H2 経路が未修正だった。

## 影響

- gRPC over HTTP/1.1・HTTP/2 が実質 UNIMPLEMENTED（既存 E2E は HTTP 200 のみ検証しており見落とし）
- F-92 の framing / streaming / trailer 詳細テストが失敗

## 調査メモ

- `extract_path_prefix` が `/grpc.test.v1.TestService/*` → prefix `/grpc.test.v1.TestService`
- `handle_proxy` / HTTP/2 proxy が prefix 除去後に `build_sub_path` → `/UnaryCall`
- 直接 9004 へフルパスで送ると `grpc-status: 0` で成功
- B-39 コメント: 「`/*` プレフィックス除去で UNIMPLEMENTED になる問題」

## 改修案

1. `compute_upstream_path(..., preserve_full_path)` を proxy に追加
2. Content-Type が `application/grpc*` のときフルパス保持（H1 `handle_proxy`・H2 `handle_http2_proxy`・ストリーミング経路）
3. 単体テスト `test_b40_*` + F-92 E2E 再実行

## 関連

- B-39（HTTP/3 gRPC フルパス）
- F-92

## 対応状況

**完了**（2026-07-10、fix/test）

- `compute_upstream_path(..., preserve_full_path)` + gRPC Content-Type 判定
- H1 `handle_proxy` / H2 `handle_http2_proxy` / ストリーミング経路でフルパス保持
- H2 経路で `upstream_group.use_h2c()` を参照（ルート use_h2c が効く）
- H2C 送信ヘッダ名の小文字化（HPACK InvalidUtf8 回避）
- 単体 `path_tests::test_b40_*`、F-92 E2E framing/streaming/trailer PASS
