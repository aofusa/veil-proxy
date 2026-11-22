# B-486: gRPC のヘルスチェックが TLS では HTTP/1.1 で送り、TLS の gRPC サーバを確かめられない

**状態: 修正済み（feat/protocol-parity。2026-11-21 起票・2026-11-22 修正。機能 × プロトコルの網羅性の確認）**

## 出典

機能とプロトコルの対応の確認（`docs/artifacts/protocol-parity/audit.md` の #6）。

## 事象

- `check_type = "grpc"` のヘルスチェック（`upstream::perform_grpc_health_check`）は、平文の上流には h2c で
  `grpc.health.v1.Health/Check` を送り（F-97）、失敗したら HTTP/1.1 の POST に落とす。TLS の上流には ALPN を出さずに
  HTTP/1.1 の POST だけを送る。
- gRPC は HTTP/2 の上でしか動かない。TLS の gRPC サーバ（grpc-go 1.67 以降は ALPN の `h2` が無いクライアントを
  ハンドシェイクで拒否する）は常に不健全になり、ヘルスチェックを付けた TLS の gRPC の上流グループは全部外れる。
- `load-balancing.md` は「HTTP/1.1 の枠で送る。HTTP/2 の gRPC は対応していない」と書く（平文は F-97 で h2c 済みで、
  文書が古い）。

## 改修案

- TLS の上流には ALPN `h2`・`http/1.1` を出し、`h2` が選ばれたら h2c と同じ HTTP/2 のやり取り（プリフェース・SETTINGS・
  HEADERS・DATA、トレーラの `grpc-status`）を TLS の上で行う。h2c の関数を `Read + Write` の上で動く形にして共有する。
  `http/1.1` が選ばれたら今の HTTP/1.1 の経路（gRPC-Web 互換のモック）。
- 文書（load-balancing、英日）を直す。

## 対応（2026-11-22）

- h2c のやり取りを `Read + Write` の上で動く `grpc_health_check_over_h2` に切り出し、平文（h2c）と TLS で共有する
  （`:scheme` は `http`・`https`）。
- TLS の上流には ALPN `h2`・`http/1.1` を出す（`http2` feature のとき）。`h2` が選ばれたら HTTP/2 で確かめ、応答を
  解釈できなければ不健全（同じ TLS の接続で HTTP/1.1 へは落とさない）。`http/1.1` が選ばれたか ALPN が無ければ従来の
  HTTP/1.1 の POST。
- 文書（`load-balancing.md`、英日）: HTTP/2 で確かめること・TLS では ALPN で決めること・HTTP/1.1 のモックへの扱いを書いた。

## 受け入れ条件

- 単体: TLS の HTTP/2 の gRPC のヘルスサーバ（テストの中で立てる。SERVING・NOT_SERVING）に対して健全・不健全を正しく判定する。
  ALPN で `http/1.1` を選ぶサーバには今と同じ判定。

## 関連

- [F-97](../features/F-97-http3-grpc-app-layer-coverage.md)、[B-478](B-478-health-check-tls-handshake-timeout-loop.md)
