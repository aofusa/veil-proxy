# B-170: tokio の shim の `Take` が `AsyncRead + Unpin` の読み手しか受けず、tower-http の `ServeDir` がコンパイルできない

**状態: 修正済み（v0.10.0）**

## 出典

F-267 の tower のサンプル（`examples/api/rust-tower-recipes`）で、tower-http 0.6 の `ServeDir` を `App::tokio_service` に
載せたらコンパイルできなかった（2026-10-26）。

## 事象

- `veil::tokio`（`crates/veil-tokio/tokio-shim` 経由の `tokio`）の `io::Take<R>` は `impl<R: AsyncRead + Unpin> AsyncRead`
  だった。tokio 本家は `R: AsyncRead` で、pin-project で `inner` を構造的にピン留めする。
- tower-http の `ServeDir` は Range の応答で `AsyncReadExt::take` を Unpin でない読み手に使うので、`Take<..>: AsyncRead` を
  満たさずエラーになる。tokio の意味論と違い、tokio 前提のライブラリが使えない。

## 修正

- `impl<R: AsyncRead> AsyncRead for Take<R>` にし、`poll_read` で `inner` を構造的にピン留めして呼ぶ（`unsafe` の
  `get_unchecked_mut`・`Pin::new_unchecked`。`Take` は `Drop`・`Unpin` を手で実装せず、`inner` を動かすメソッドは
  `&mut self`・`self`（ピン留めされていない `Take`）だけ、という不変条件をコメントに書いた）。

## 試験

- 単体: `src/tokio/io/util.rs` の `take_reads_not_unpin_reader_up_to_limit`（`PhantomPinned` の読み手を上限まで読む）。
- E2E: `tests/api/library_e2e.sh kit` の tower のサンプル（`ServeDir` の `/files/a.txt`）。
