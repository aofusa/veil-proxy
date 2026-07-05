# B-21: HPACK Huffman デコーダが不正入力でシフト量オーバーフロー panic する

## 出典

`cargo fuzz run hpack_decode`（F-52）で検出（2026-07-05）。**同日修正済み。**
クラッシュ入力: `fuzz/artifacts/hpack_decode/crash-1447d26f0d35748fff13c48577206bc40c2227c3`
（`94 01 94 01 ff ff ... ff 01 00 00 00 f9`）。

## 事象

`src/http2/hpack/huffman.rs` の `huffman_decode` は、入力バイトを 64bit レジスタ
`bits` へ順次シフトインしながら符号表と照合する。どの Huffman 符号にも一致しない
ビット列が続くと `found = false` で内側ループを抜けて次バイトを読むが、
**`bits_left`（保持ビット数）を減算しないまま単調増加**する経路があった。

`bits_left` が 64 以上になると、照合時の
`let shift = bits_left - len; let extracted = (bits >> shift)` で
シフト量が 64 以上になり、Rust のシフトオーバーフローで **panic**
（`huffman.rs:326:37`、`attempt to shift right with overflow`）。

不正な HPACK Huffman 文字列は HTTP/2 リクエストヘッダとして**外部から送信可能**
なため、リモートからワーカータスクを panic させられる DoS 面のリスクがあった
（タスク単位で捕捉されプロセスは継続するが、当該接続は異常終了）。

## 修正内容（2026-07-05）

HPACK Huffman 符号の最長は 30 ビット（RFC 7541 Appendix B）。この不変条件を使い、
`found = false` かつ `bits_left >= 30`（= 最長符号長以上のビットがあるのに
どの符号にも一致しない）場合は不正入力として `HuffmanDecodeError` を返すガードを追加。
これにより `bits_left` は常に 37 未満に保たれ、シフト量が 64 に達しない。

回帰テスト（`huffman::tests`）:
- `test_huffman_decode_invalid_no_panic`: クラッシュ入力そのもの + 全 0 の長い列で
  panic しないこと、デコード不能入力が `Err` になること
- `test_huffman_roundtrip_decode`: 正当な符号列のラウンドトリップ（デコード経路の
  正当性が壊れていないことの担保）

## 関連

- 検出: [F-52](../features/F-52-cargo-fuzz-libfuzzer.md)（cargo-fuzz）
