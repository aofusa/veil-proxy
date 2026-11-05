# B-302: `Accept-Encoding` の交渉が q=0 を無視し、クライアントが受け付けない方式・設定に無い方式を返す。要求ごとに小文字化で確保する

**状態: 修正済み（v0.11.0）**

## 事象（コードの読み取り。静的配信とプロキシの全プロトコルに効く）

1. `AcceptedEncoding::parse`（`src/config/compression.rs:50-76`）: 初期値 `(Identity, 0.0)` と q=0 の zstd・br が同点になり、同点の優先で zstd・br が勝つ
   （`Accept-Encoding: br;q=0` に br を返す）。`gzip;q=0, *` は gzip を選ぶ。`split_once(";q=")` なので `gzip; q=0.5`（空白あり）は q を読めず 1.0 になる。
2. `should_compress`（`compression.rs:310-359`）は「zstd を受け付けるクライアントは br・gzip・deflate も受け付ける」とみなしている。
   - `preferred_encodings = ["br","gzip"]`（文書の静的配信の例）で `Accept-Encoding: zstd` に br を返す。
   - `preferred_encodings = ["gzip"]` で br だけのクライアントに gzip を返す。
   - `"deflate" => Some(Deflate)`（:352）はクライアントに関係なく deflate を返す。
   - :358 の既定の分岐は `preferred_encodings` に無い方式を返す（文書の「この中から選ぶ」と合わない）。
3. `to_ascii_lowercase()`（:42）が `Accept-Encoding` のある要求ごとに確保する（圧縮が無効なルートでも。`forward.rs:35`・`h2.rs:1836`・
   `request_task.rs:615`・`http3_server/mod.rs:1741`）。ホットパス規則に反する。

## 影響

- 受け付けない方式で圧縮した本文を返す（クライアントが読めない）。P2（多くのブラウザは br・gzip・zstd を全部送るので表に出にくい）。

## 改修案

- `src/api/compress.rs:53-125` の、確保しない正しい `accepted_q`/`parse_q`/`negotiate`（大文字小文字を区別しない比較）を `config` へ移し、静的配信・プロキシ・
  ライブラリで共有する。`should_compress` は「クライアントが q>0 で受け付ける ∩ `preferred_encodings`」の中から q の最大、同点は設定順で選ぶ。
- 単体試験の表（q=0、`*`、`*;q=0`、空白、大文字、`identity;q=0`、設定に無い方式）。allocs/req が減ることを確かめる。
- [F-294](../features/F-294-precompressed-static-siblings.md) はこの解析器を使う。

## 修正

- `config::ClientAccept`（`src/config/compression.rs`）: `Accept-Encoding` を確保せずに 1 回走査し、方式ごとの q（千分率）を持つ。大文字小文字・`;` 前後の空白・`*`・`q=0`・`x-gzip` を扱う。
- `should_compress` は「クライアントが q>0 で受け付ける ∩ `preferred_encodings`」の中で q 最大、同点は設定順（`ClientAccept::negotiate`）。設定に無い方式は返さない。
- 静的配信・プロキシ（HTTP/1.1・HTTP/2・HTTP/3）とライブラリ（`src/api/compress.rs`）が同じ解析器を使う。`AcceptedEncoding::parse`（要求ごとの `to_ascii_lowercase`）は削除。
- 試験: `config::compression::client_accept_tests`、`legacy_bin_tests::encoding_tests`。

## 関連

- [F-294](../features/F-294-precompressed-static-siblings.md)、[B-297](B-297-zero-copy-compression-docs-mismatch.md)（D4）
