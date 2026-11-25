# B-498: `[tls] cipher_suites` の記載順がサーバの優先にならない（rustls の `ignore_client_order` を立てていない）

**状態: 完了（2026-11-25、feat/httparena-perf。同日起票。F-320 の調整〈暗号スイートの順〉を調べていて見つけた）**

## 対応（2026-11-25）

- `load_tls_config`（`src/config/tls_server.rs`）で、`cipher_suites` が空でないときに `ServerConfig::ignore_client_order = true`。
  起動時・設定のリロード・証明書のリロード（`build_server_config_from_paths`）は全部この関数を通る。空のときは今までどおりクライアントの順。
- HTTP/3: Linux の quiche（aws-lc）の TLS 1.3 の選び方（`ssl_choose_tls13_cipher`）は、サーバの一覧を「許すかどうか」にだけ使い、
  順はクライアントの順（AES の命令の無い機械では ChaCha20 を優先）。サーバの順を使う口が無いので揃えられない。文書（英日の tls.md・
  configuration.md・`examples/config.toml`）に「HTTP/1.1・HTTP/2 はサーバの順、HTTP/3 は一覧の中からクライアントの順」と書いた。
- 試験: 単体 `config::cipher_suites_tests::negotiation_uses_server_order_only_when_listed`（メモリの中の rustls のハンドシェイク。クライアントは
  ChaCha20 → AES-128 → AES-256 の順で送り、一覧ありはサーバの先頭、一覧なしはクライアントの先頭）。E2E の
  `test_tls_cipher_suites_server_preference` はクライアントを AES-128 → AES-256 の順にした（直す前の veil では AES-128 が選ばれて落ちる）。

## 事象

- 文書は `[tls] cipher_suites` の「記載順 = サーバ優先度順」と書いている（`examples/config.toml` は「rustls はサーバ選好で
  ネゴシエートする」、`docs/guide/configuration.md`・`docs/guide/ja/configuration.md` の表、`src/config/tls_server.rs` のコメント、F-50）。
- しかし veil は CryptoProvider の `cipher_suites` を設定順に差し替えるだけで（`src/config/tls_server.rs` の `load_tls_config`）、
  rustls の `ServerConfig::ignore_client_order` を立てていない。rustls 0.23（0.23.46 で確認）は既定（`false`）で、**クライアントの
  一覧の順に見て、サーバが許す最初のスイート**を選ぶ（`rustls/src/server/hs.rs`）。設定の順は「許すかどうか」にしか効いていない。
- 例: `cipher_suites = ["TLS13_AES_128_GCM_SHA256", "TLS13_AES_256_GCM_SHA384", ...]` としても、OpenSSL の既定の順
  （AES-256-GCM → ChaCha20 → AES-128-GCM）を送る curl・wrk 等には AES-256-GCM が選ばれる。
- E2E の `test_tls_cipher_suites_server_preference`（`tests/e2e_tests/tickets.rs`）は、rustls の既定のクライアント
  （AES-256 → AES-128 → ChaCha20）と、同じ順の先頭（AES-256）を置いたサーバの設定で確かめているので、どちらの順で選んでも通る。

## 直し方

1. `cipher_suites` が空でないときは `ServerConfig::ignore_client_order = true` にする（nginx の `ssl_prefer_server_ciphers on` と同じ）。
   空（rustls の既定の一覧）のときは今のまま（クライアントの順。AES の命令を持たないクライアントが ChaCha20 を選べる）。
   証明書のリロードで作り直す `ServerConfig` も同じ関数を通ることを確かめる。
2. HTTP/3: Linux の quiche（aws-lc）は TLS 1.3 の一覧を `SSL_CTX_set_ciphersuites` で渡している（F-314）。aws-lc がその順で選ぶか、
   クライアントの順で選ぶかを確かめ、HTTP/1.1・HTTP/2 と揃える（揃えられなければ文書に書く）。Linux 以外（BoringSSL）は
   TLS 1.3 の順を変えられない（今の文書のとおり）。
3. 試験: E2E の `test_tls_cipher_suites_server_preference` を、クライアントがサーバと違う順（例: ChaCha20 → AES-128 → AES-256）を
   送る形に直す。単体試験（メモリの中の rustls のハンドシェイク。証明書は `rcgen`）で「一覧あり → サーバの順」「空 → クライアントの順」。
4. 文書: `docs/guide/configuration.md`（英日）・`examples/config.toml` に「一覧を書いたときはサーバの順、書かないときはクライアントの順」と書く。

## 影響

- 運用者が AES-128-GCM（CPU が軽い）や ChaCha20 を先頭にしても効いていなかった。安全性は変わらない（選ばれるのは許した
  スイートだけ）。
- HttpArena: wrk（json-tls・static-tls、OpenSSL 3.0）は AES-256-GCM、h2load（HTTP/2・HTTP/3）は nghttp2 の既定の一覧の先頭の
  AES-128-GCM になっている（F-320 の 8）。

## 受け入れ条件

- 上の試験が io_uring・epoll で通る。kTLS（AES-GCM のみ）の経路も同じ順で選ぶ。`cargo test`・E2E。

## 関連

- F-50・F-314・[F-320](../features/F-320-httparena-veil-entry-config.md)
