# B-01: io_uring accept が O_NONBLOCK を設定しない

## 事象（再現手順）

`client_body_timeout_secs` を設定したルートへ、Content-Length より少ないボディを送った後に黙り込むクライアントを接続した場合、タイムアウトが発火せず 15 秒後に接続が切れる（ntest のデフォルトタイムアウトで強制終了）。

再現テスト: `test_buffering_client_write_timeout`

## 影響

- `client_body_timeout_secs` が機能しない → スローロリス系の攻撃で接続を長時間占有される。
- 影響範囲: io_uring ドライバ使用時（Linux + `--features iouring`）。

## 調査メモ

monoio の `IORING_OP_ACCEPT` は `accept4(2)` と異なり `SOCK_NONBLOCK` フラグを受理したソケットに付与しない。そのため `KtlsServerStream::read` 内の `raw_read(fd, ...)` （直接 `libc::read`）が **ブロッキングソケット** に対してブロッキング呼び出しになる。

`monoio::select!` の timer ブランチは、イベントループが `park()` する機会を得られないため発火しなかった。

## 改修案（実施済み）

`ktls_rustls.rs` の `accept()` および `connect()` 関数冒頭で `fcntl(F_GETFL/F_SETFL O_NONBLOCK)` を呼び出し、`raw_read`/`raw_write` を使う前にノンブロッキングを保証する。

```rust
let fd = stream.as_raw_fd();
let flags = unsafe { libc::fcntl(fd, libc::F_GETFL, 0) };
if flags >= 0 && (flags & libc::O_NONBLOCK) == 0 {
    unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
}
```

## 対応状況

完了（2026-06-20）。`src/ktls_rustls.rs` の `accept()` / `connect()` を修正。
