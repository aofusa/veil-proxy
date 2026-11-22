# F-314: HTTP/3 にも `[tls] cipher_suites`・`session_resumption` を効かせ、チケット鍵を全ワーカーで共有する

**状態: 完了（2026-11-22、feat/protocol-parity。2026-11-21 起票。機能 × プロトコルの網羅性の確認）**

## 今の状態

- `[tls] cipher_suites` は TCP の TLS だけ（`examples/config.toml` に「HTTP/3 は対象外」）。HTTP/3 は TLS 1.3 の全部の暗号
  （AES-128-GCM・AES-256-GCM・ChaCha20-Poly1305）を受ける。準拠のために暗号を絞った構成でも HTTP/3 だけ絞られない。
- `[tls] session_resumption`（F-183）も TCP だけ。HTTP/3 は quiche（BoringSSL）の既定で、セッションチケットの鍵が
  ワーカーの `SSL_CTX` ごとに別で、証明書のリロードで `Config` を作り直すと変わる。再接続が別のワーカーへ着くと再開できず
  （完全なハンドシェイク・0-RTT も使えない）、`"off"` でも HTTP/3 は再開する。

## 改修内容

- 暗号: `cipher_suites` の TLS 1.3 の名前を HTTP/3 の `SSL_CTX` にも入れる（TLS 1.2 の名前は QUIC では使わない）。
  使えない環境（設定する口の無い TLS ライブラリ）では、絞った構成で HTTP/3 を有効にしたら起動エラーにする（黙って無視しない）。
- 再開: 起動時に作るプロセス共有の鍵を全ワーカーの quiche の `Config` に入れる（`Config::set_ticket_key`）。証明書の
  リロードでも同じ鍵を使う。`"off"` ではチケットを出さない。
- 文書（tls・configuration・examples、英日）。

## 受け入れ条件

- 統合: `cipher_suites = ["TLS13_AES_256_GCM_SHA384"]` で、AES-128 だけを出す HTTP/3 のクライアントが拒否される。
- 統合: 1 つ目の接続で受けたチケットで、別のワーカー（別の `Config`）への 2 つ目の接続が再開する。証明書のリロードの後も再開する。
  `"off"` では再開しない。

## 対応

- 設定: `[tls]` から HTTP/3 への写し（`config::H3TlsSettings`。TLS 1.3 の暗号の OpenSSL の名前の一覧・チケットの数）を、
  設定のロードで `Http3ConfigSection` の `#[serde(skip)]` の `tls` に解決する（`config::h3_tls_settings`。HTTP/3 のリスナーが
  あるときだけ。`validate_config` でも同じ検査）。TLS 1.3 の名前が無い一覧はエラー。Linux 以外は 3 つ全部を並べた一覧だけを
  通す（BoringSSL は TLS 1.3 の暗号を絞れない。AWS-LC は `SSL_CTX_set_ciphersuites` で絞れ、TLS 1.3 のサーバは暗号を選ぶ
  ときにその一覧だけを見る）。`"off"` はチケット 0 枚、`"cache"` も HTTP/3 はチケット（BoringSSL の TLS 1.3 はサーバ側の
  セッションキャッシュを持たない）。
- vendoring の quiche（`README.veil.md` の 9。0.24.9-veil.5 は未公開なので版はそのまま）: `Config::set_session_tickets`
  （`SSL_CTX_set_num_tickets`、0 は `SSL_OP_NO_TICKET`）と、`aws-lc-sys` feature だけの `Config::set_tls13_ciphersuites`。
- ワーカー（`configure_quic_tls`）: 起動時と、Linux 以外の証明書のリロード（`Config` を作り直す）で暗号・チケットの数・
  共有のチケット鍵を入れる。Linux のリロードは同じ `SSL_CTX` の証明書だけを差し替えるので残る。
- チケット鍵: プロセス共有（`H3_TICKET_KEY`、48 バイト）。自分で入れた鍵は TLS ライブラリが作り直さないので、TCP の rustls と
  同じ 6 時間で世代を進め、最初に気づいたワーカーが乱数で作り直す（6 時間に 1 回だけ取るロック）。メインループは周回ごとに
  既にある `now` と次の世代の時刻を比べるだけ（受理の経路・要求ごとの経路には足していない）。作り直す前に出したチケットは
  使えなくなる（rustls のように前の鍵を残すには `SSL_CTX_set_tlsext_ticket_key_cb` が要る。1 回のフルハンドシェイクで済むので
  入れていない）。

### 試験

- 単体 `config::tls_server::f314_h3_tls_tests`（5 件）: 既定・再開の方式ごとのチケットの数・TLS 1.3 の名前の順・
  絞った一覧（Linux は通り、他の OS は拒否）・TLS 1.2 だけの一覧の拒否。
- 自己完結型 E2E `protocol_parity_tests::f314_http3_session_resumption_across_workers`: ワーカー 2 本で、クライアントの
  ポートを変えて続けて張る 10 本の接続が全部 0-RTT で再開する。証明書のリロード（チケットの無いクライアントで新しい証明書を
  確かめる）の後も前のチケットで 4 本再開する。`"off"` は 3 本とも再開しない。
- `f314_http3_tls13_cipher_suites`（Linux）: `cipher_suites = ["TLS13_AES_256_GCM_SHA384", ...]` で、AES-128 だけの
  クライアントは HTTP/3 で断られ、AES-256 だけのクライアントには 200。
- 否定の確認: 鍵を共有しない版は 2 本目の接続で再開に失敗した。暗号とチケットの数を入れない版は AES-128 のクライアントに
  200 を返し、`"off"` の 2 本目が再開した。
- clippy（`full`・`full,epoll`、`--lib --no-default-features --features http3`、`--lib --no-default-features`）。Linux 以外の
  経路（`set_tls13_ciphersuites` を呼ばない・リロードで `configure_quic_tls`）はこの端末でビルドできない（mac に繋がらない）。

## 関連

- [F-183](F-183-veil-allocator-and-library-tuning.md)・[F-105](F-105-http3-cert-hot-reload.md)・[F-280](F-280-security-review-info-hardening.md)（0-RTT）
