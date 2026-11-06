# B-313: Linux の kTLS で TLS 1.2（AES-GCM）の送信の最初のレコードが Finished と同じ nonce を使う

**状態: 修正済み（v0.11.0）**

## 事象

v0.11.0 前のセキュリティスキャン（Claude Security、F32）で判明。`[tls] ktls_enabled = true` で TLS 1.2 の AES-GCM を
交渉した接続（クライアント側も、TLS 1.2 の上流への kTLS も）で、カーネルが送る最初のレコードが、rustls がハンドシェイクの
最後に送った Finished（seq 0）と同じ（鍵, nonce）で暗号化される。GCM で nonce が重なると、2 つのレコードのタグから
認証鍵 H が求まり（forbidden attack）、経路上の攻撃者がその後のレコードを改ざん・偽造できる。最初の応答の平文 16 バイトも
Finished との XOR で漏れる。

## 原因

TLS 1.2 の AES-GCM の nonce は `salt(4) || explicit(8)`。rustls はレコードごとに `explicit_base XOR seq` を使い
（seq 0 の Finished は `explicit_base`）、`dangerous_extract_secrets` は `salt || explicit_base` を返す。
`src/ktls.rs` の `build_aes{128,256}_gcm_info` はこの `explicit_base` をそのまま `tls12_crypto_info.iv` に渡していた。
Linux の kTLS は TLS 1.2 では `iv` を最初のレコードの explicit nonce に使い、レコードごとに 1 ずつ増やす
（TLS 1.3 は `iv XOR rec_seq` で rustls と一致するので問題ない）。

## 修正

TLS 1.2 の送信方向だけ、`iv` を CSPRNG（aws-lc-rs）の 8 バイトにする（OpenSSL と同じ）。explicit nonce はレコードに
載って相手へ届くので相手側の変更は要らない。受信方向はカーネルがレコードから読むので変えない。FreeBSD の kTLS は
TLS 1.2 の explicit nonce にレコードの通し番号を使うため（`explicit_base` を使わない）対象外。

単体試験 `ktls::tests::test_tls12_tx_explicit_iv_is_not_rustls_base`。

## 関連

- docs/artifacts/security/v0110-claude-security/（F32）
