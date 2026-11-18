# B-466: メッシュの Zipkin のスパンを、コレクタのクラスタの TLS（Istio の auto-mTLS 等）を使わずに平文で送っていた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Zipkin spans sent in cleartext, ignoring the collector cluster's TLS/mTLS transport socket」
から確認。

## 事象

- `mesh::tracing::report_loop` はコレクタのクラスタからエンドポイントのアドレスだけを取り、`std::net::TcpStream` で
  スパンの JSON を平文の HTTP で送っていた。クラスタの `transport_socket`（`tls_default`・`tls_matches`）を見ず、
  `https://` のコレクタの URI も `http://` と同じに扱った。スパンは要求の URL（クエリのトークンを含む）・メソッド・
  状態・タグにした要求ヘッダを運ぶので、ポッドのネットワーク・出口の経路で読めた。

## 対応

- エンドポイントに合うクラスタの TLS（`ClusterCfg::tls_for`）があれば、データプレーンの HTTP/1.1 の要求と同じ
  `rustls::ClientConfig`（`UpstreamTls::h1`。SAN の検査・ALPN を含む）と名前（`upstream::server_name`）で TLS を張って送る。
- コレクタの URI が `https://` でクラスタに TLS が無ければ送らない（1 回だけ警告する）。

## テスト

- `mesh::tracing::tests::spans_use_the_collector_cluster_tls`（自己署名の証明書の TLS のサーバで、POST の本文が
  TLS で届くこと。https の URI で TLS の無いクラスタには接続しないこと）。
