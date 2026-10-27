# B-228: `SafeReadBuffer::into_inner` が未初期化のバイトを含む `Vec` をそのまま返せる（型で防げていない）

**状態: 修正済み（v0.10.0）**

## 出典

2026-10-27 のセキュリティレビュー（v0.9.2 の `src/pool.rs` の読解）。

## 事象

- `SafeReadBuffer::{new, from_vec}`・`buf_put_vec` は `Vec::with_capacity(cap)` の直後に `set_len(cap)` して未初期化の領域を `len` に含める設計。
- `as_valid_slice()` 経由なら `valid_len` で切るので安全だが、`into_inner()` は未初期化のバイトを含む `Vec` をそのまま返す（rustdoc に警告はある）。
  呼び出し側が `len()` で扱うと、未初期化のメモリ（前の要求の残り）を送る情報漏えいになる。
- 今の呼び出し側（`buf_put`）は安全だが、誤用を型で防げていない。`IoBufMut::set_init` は `valid_len` も更新していて、`BytesMut`・`OffsetBufMut` の
  実装は契約を doc とテストで固定している。

## 改修案

- `into_inner` を消して `into_truncated`（`valid_len` で `truncate` した `Vec`）だけにするか、`pub(crate)` + `#[must_use]` にして
  `tools/lint/` で呼び出し元を検査する。
- 長期的には `MaybeUninit<u8>` ベースへ移す（`set_len` を `set_init` の後にしか呼ばない）。

## 受け入れ条件

- `grep -rn "into_inner()" src/` で `SafeReadBuffer` の呼び出し元が無いか、すべて `valid_len` で切っている。性能の交互 A/B が変わらない。

## 修正（v0.10.0）

- `SafeReadBuffer::into_inner`（pub）を `pub(crate) fn into_pool_vec`（`#[must_use]`、`buf_put` だけが使う）に替え、クレートの外から未初期化のバイトを含む `Vec` を取り出せないようにした。データとして取り出すのは `into_truncated`（有効長で切る）だけ。rustdoc の例も `into_inner` を使わない形に直した。
- 試験: `safe_read_buffer_tests::into_truncated_returns_only_valid_bytes`。
- `MaybeUninit` ベースへの移行は残す（今の呼び出し側は有効長より先を読まない）。

## 関連

- [F-29](../features/F-29-lockfree-cache-and-async-fs.md)、[B-07](B-07-iouring-future-drop-uaf.md)
