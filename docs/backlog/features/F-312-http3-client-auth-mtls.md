# F-312: HTTP/3 でもクライアント証明書（`[tls] client_auth`）を要求・検証する

**状態: 未着手（2026-11-21 起票。機能 × プロトコルの網羅性の確認、feat/protocol-parity）**

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

## 関連

- [F-217](F-217-mesh-workload-mtls.md)・[F-220](F-220-mesh-authorization.md)・[F-105](F-105-http3-cert-hot-reload.md)
