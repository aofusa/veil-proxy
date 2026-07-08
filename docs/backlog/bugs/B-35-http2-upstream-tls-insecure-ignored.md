# B-35: HTTP/2 上流 HTTPS 接続で tls_insecure が無視される

## 事象

- E2E 環境で `tls_insecure = true` の upstream（`backend-pool`）へ、HTTP/2 クライアント（ALPN h2）からのリクエストが **502 Bad Gateway** になる。
- HTTP/1.1 クライアント（`curl --http1.1`）では **200 OK**。
- プロキシログ: `[HTTP/2] TLS handshake error: invalid peer certificate: UnknownIssuer`

## 再現手順

1. `./tests/e2e_setup.sh start`
2. `curl -ks --http2 https://127.0.0.1:8443/` → 502
3. `curl -ks --http1.1 https://127.0.0.1:8443/` → 200

## 影響

- HTTP/2 フロントエンド経由の全 E2E テストが連鎖失敗（214 件規模）。
- 本番で HTTP/2 クライアント + 自己署名/社内 CA 上流の構成が使用不可。

## 原因

`handle_http2_proxy_https` および H2 リクエストボディストリーミング経路が `get_tls_connector()` 固定で、`upstream_group.tls_insecure()` を参照していなかった（HTTP/1.1 経路は `connect_https_backend_fresh` で正しく分岐）。

## 改修

- `handle_http2_proxy_https` に `tls_insecure` 引数を追加し、`get_tls_connector_insecure()` を使用。
- H2 ストリーミングアップロード経路も同様に `upstream_group.tls_insecure()` を参照。

## 対応状況

**完了**（fix/e2e ブランチ、コミットで修正済み）