# B-258: ハンドシェイク直後に rustls のデフレーマに部分レコードが残ったまま kTLS RX を有効化すると境界がずれて接続が切れる

**状態: 未修正（2026-10-28 起票。F-282 の 2 番目の項目を切り出した）**

## 出典

2026-10-27 のセキュリティレビュー 2 回目の KT-02（Info）を、3 回目で veil と rustls 0.23.46 の実装まで追って確定した
（`docs/artifacts/security/analysis/08-kernel-zero-copy-deep-dive.md`、git 管理外）。

## 事象

- `src/ktls_rustls.rs::do_server_handshake` は `raw_read` で最大 16KB をまとめて読み、全部を `read_tls_all` で rustls へ渡す。
- クライアントが TLS 1.3 の最後のフライト（Finished）と最初のアプリケーションデータを同じセグメントで送り、その読み取りがレコード境界で切れないと、
  未完のレコードの先頭部分が rustls のデフレーマに残る。
- rustls の秘密抽出（`dangerous_into_kernel_connection`）は「ハンドシェイク完了」「`sendable_tls` が空」しか検査せず、受信側の未処理バイトは見ない。
  veil の `drain_rustls_plaintext` は復号済みの平文だけを取り出すので、残った暗号文は `setsockopt(TLS_RX)` の後に失われ、カーネルはレコードの途中から
  解析して復号に失敗する（`read(2)` が `EBADMSG`/`EINVAL`）→ その接続は切れる。他の接続への影響は無い。
- 完全なレコードが同じ読み取りに含まれていた場合は rustls が復号して `drained` に入り、進んだシーケンス番号が抽出されるので正しく動く。

## 影響

- P3（可用性、`ktls = true` のときだけ）: 対象のクライアント実装（最後のフライトと要求を 1 セグメントで送り、かつ MSS の境界がレコードをまたぐ）の
  接続が最初の要求で切れる。クライアント側のリトライで隠れるため、観測は「kTLS を有効にすると稀に接続が切れる」になる。

## 改修案（どちらか）

1. ハンドシェイク中の読み取りをレコード境界に合わせる: `MSG_PEEK` で 5 バイトの TLS レコードヘッダの長さを読み、完全なレコードの分だけ `read` して
   rustls へ渡す（部分レコードを rustls に入れない）。ハンドシェイク中だけなので性能への影響は無い。
2. 部分レコードが残る可能性があれば kTLS RX を諦める: rustls の `unbuffered` API（消費バイト数を返す）でハンドシェイクを進め、未消費バイトがあれば
   `TlsMode::KtlsTxOnly` で続ける。

## 受け入れ条件

- kTLS の E2E に「クライアントが最後のフライトと最初の要求を 1 セグメントで、レコード境界をまたぐ位置で送る」ケースを足し（`tests/e2e_tests/` に
  TCP の中継を 1 つ置き、届いたバイトを溜めて境界をまたぐ位置で 2 回に分けて送る）、200 が返ること。

## 関連

- [F-282](../features/F-282-security-review-followup-hardening.md)（KT-01 の制御レコードの扱いは残る）、[B-86](B-86-http3-tls-backend-plaintext-overflow.md)（`read_tls` ごとの排出）
