# B-229: kTLS の `CryptoInfo`（鍵・IV・シーケンス）を `setsockopt` に渡した後にゼロ化していない

**状態: 未修正（2026-10-27 起票。セキュリティレビュー ZC-02）**

## 出典

2026-10-27 のセキュリティレビュー（v0.9.2 の `src/ktls.rs`・`src/ktls_rustls.rs` の読解）。

## 事象

- `ktls::setup_tls_info` / `extract_tx_rx`、`ktls_rustls::setup_ktls_after_ulp` は `CryptoInfo`（`tls12_crypto_info_*`）を `setsockopt(TLS_TX/TLS_RX)` に
  渡した後、構造体をゼロ化せずに drop する（`zeroize` の使用なし）。
- 鍵はスタック・ヒープに残り、同じ領域を再利用した別の経路へ混入し得る。単体では攻撃に直結しないが、`tls_reload.rs` が秘密鍵を
  `secure_zero_vec` でゼロ化している方針と不整合。

## 改修案

- `CryptoInfo` に `Drop` で `zeroize`（`std::ptr::write_volatile` + `compiler_fence`。`tls_reload::secure_zero_vec` と同じ実装）を入れる。
  kTLS の有効化は接続ごとに 1 回なのでホットパスの負荷は無視できる。

## 受け入れ条件

- 単体: `setup_tls_info` の後に構造体の領域がゼロであること（`Drop` の後の読み出しは UB なので、`zeroize` を明示的に呼ぶ関数を試験する）。

## 関連

- [F-105](../features/F-105-http3-cert-hot-reload.md)（秘密鍵のゼロ化）
