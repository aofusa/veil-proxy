# F-312: HTTP/3 でもクライアント証明書（`[tls] client_auth`）を要求・検証する

**状態: 完了（2026-11-22、feat/protocol-parity。2026-11-21 起票。機能 × プロトコルの網羅性の確認）**

## 今の状態

- `[tls] client_auth = "optional" | "require"`・`client_ca`・`client_allowed_sans`（F-217・F-220）は TCP の TLS（HTTP/1.1・HTTP/2）
  だけ。HTTP/3（`[http3] enabled` か `http3 = true` の `[[listener]]`）と組み合わせると起動エラーにしている
  （`config/validate.rs`。HTTP/3 のクライアントが検証を素通りするため）。
- mTLS の構成では HTTP/3 を使えない（HTTP/3 だけのリスナーにも mTLS を掛けられない）。

## 改修内容

- quiche の `Config` で `verify_peer(true)` にし、`client_ca` を信頼アンカーに読む（Linux は証明書と同じく memfd 経由の
  `load_verify_locations_from_file`、他の OS は BoringSSL の `SSL_CTX` の証明書ストアへ直接入れる）。BoringSSL は
  `SSL_VERIFY_PEER` で、送られた証明書を `client_ca` で検証し、検証できなければハンドシェイクを失敗させる。
- `"require"` で証明書を送らなかった接続・`client_allowed_sans` に合わない接続は、ハンドシェイクの完了を確かめた時点
  （HTTP/3 の層を作る前。0-RTT の要求も処理する前）で `CONNECTION_CLOSE`（暗号のエラー: `certificate_required`・
  `access_denied`）で閉じる。SAN の照合は TCP と同じ `x509_san::parse_sans` で 1 接続 1 回。
- 証明書のリロード（F-105）で作り直す `Config` にも同じ設定を入れる（起動時の `client_auth`・`client_ca`・
  `client_allowed_sans` を使い続ける。TCP と同じ）。
- 起動エラーの検査を外し、文書（tls・configuration、英日）を直す。

## 受け入れ条件

- 統合（プロセス内の quiche のクライアントとサーバ）: `require` で証明書無し・別の CA の証明書・許可外の SAN は拒否、
  正しい証明書は要求に応答する。`optional` は証明書無しでも応答し、別の CA の証明書は拒否する。
- 証明書のリロードの後も同じ。

## 対応

- 判定は TCP と同じ rustls の検証器（`client_cert_verifier` が作るもの。`client_ca` だけを信頼し、`client_allowed_sans`・
  `"require"` も見る）で行う。`config::H3_CLIENT_AUTH`（`ArcSwapOption`）に検証器と `client_ca` の PEM を置き、TCP の TLS の
  設定を組み立てるたび（起動・証明書のリロード）に差し替える。
- quiche の側は「証明書を要求させる」ためだけに設定する: Linux は `verify_peer(true)` と memfd 経由の
  `load_verify_locations_from_file`（`load_client_ca`）、他の OS は boring の `SslContextBuilder` の証明書ストアへ `client_ca` を
  入れて `SslVerifyMode::PEER`。Linux の quiche（`Config::new`）は**システムの CA も信頼する**ので、BoringSSL の検証だけでは
  公開 CA の証明書（clientAuth の EKU 付き）が通ってしまう。最後の判定は必ずハンドシェイクの後の検証器で行う。
- `Http3Handler::init_h3` で、h3 の層を作る前（要求・0-RTT の要求を 1 つも読む前）に `peer_cert_chain()` を検証器へ渡す。
  通らなければ `CONNECTION_CLOSE`（暗号のエラー 0x100 + アラート: 証明書無し＝`certificate_required`(116)、
  `client_allowed_sans` の外＝`access_denied`(49)、それ以外＝`bad_certificate`(42)）で閉じ、閉じている間は確かめ直さない
  （`local_error()`）。セッションの再開（PSK）でも BoringSSL はセッションに記録した証明書を返すので、再開した接続も同じ
  検証を通る（証明書無しで始めたセッションの再開は `"require"` なら閉じる）。ログは `debug!`（相手が接続ごとに起こせる）。
- 証明書のリロード: Linux は既存の `Config` に新しい `client_ca` を足す（古い CA は BoringSSL のストアに残るが、最後の判定は
  差し替えた検証器が `client_ca` だけで行う）。他の OS は `Config` ごと作り直す。
- 起動エラー（`config/validate.rs`）を外した。`config_has_http3` は FreeBSD の capability mode の検査だけが使うので
  `cfg(all(target_os = "freebsd", feature = "http3"))` にした。
- 試験の PKI（`tests/standalone_e2e.rs` の `write_mtls_pki`）は証明書ごとに主体者名を分けた。rcgen の既定は全部同じ CN で、
  発行者と同じ名前の証明書を BoringSSL は自己発行とみなし CA へつながない（`error 48`＝unknown_ca で HTTP/3 だけ失敗した）。

### 試験

- 自己完結型 E2E `protocol_parity_tests::f312_http3_client_certificates`（quinn + rustls のクライアント証明書）:
  `require` + `client_allowed_sans` で証明書無し・別の CA・許可外の SAN を HTTP/3 と HTTP/1.1（TLS）の両方で拒否し、許可された
  証明書には 200。サーバ証明書を差し替えて HTTP/3 が新しい証明書を見せるのを待ち、リロードの後も同じ。`optional` は証明書無しに
  200、別の CA は拒否。
- 検査を外した版（検証の結果を捨てる）で、証明書無しの HTTP/3 が 200 になり試験が失敗することを確かめた。
- `cargo clippy --all-targets`（`full`・`full,epoll`）、`--lib --no-default-features --features http3`、`--lib --no-default-features`。
  Linux 以外の経路（boring）は mac に繋がらずビルドできなかったので、boring 4.22.0 のソースで `cert_store_mut`・
  `X509StoreBuilderRef::add_cert(X509)`・`SslContextBuilder::set_verify(SslVerifyMode)` の型を照合した。

## 関連

- [F-217](F-217-mesh-workload-mtls.md)・[F-220](F-220-mesh-authorization.md)・[F-105](F-105-http3-cert-hot-reload.md)
