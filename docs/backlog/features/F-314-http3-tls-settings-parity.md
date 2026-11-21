# F-314: HTTP/3 にも `[tls] cipher_suites`・`session_resumption` を効かせ、チケット鍵を全ワーカーで共有する

**状態: 未着手（2026-11-21 起票。機能 × プロトコルの網羅性の確認、feat/protocol-parity）**

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

## 関連

- [F-183](F-183-veil-allocator-and-library-tuning.md)・[F-105](F-105-http3-cert-hot-reload.md)・[F-280](F-280-security-review-info-hardening.md)（0-RTT）
