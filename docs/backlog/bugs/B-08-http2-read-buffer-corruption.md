# B-08: HTTP/2 読み込みバッファの破損（部分フレーム / compact 残留）

## 事象

B-07（io_uring Future Drop UAF）の修正で segfault が解消した後、`features full` の E2E で
**H2C / gRPC のテストが多数 502 で失敗**するようになった。proxy ログに以下が頻発する。

```
ERROR [src/proxy.rs:5371] H2C request error: Frame too large: 6828350 bytes (max: 16384)
ERROR [src/proxy.rs:5345] H2C handshake error: Connection error PROTOCOL_ERROR: DATA frame with stream ID 0
```

HTTP/2-over-TLS は全通過するのに H2C（平文 HTTP/2）だけ壊れる。

## 影響

H2C サーバ / H2C プロキシ / gRPC（H2C 経由）でフレーム境界が崩れ、巨大フレーム長や
不正フレームとして解釈され 502 になる。フレームが複数回の read にまたがる（部分フレーム）
タイミングで再現する。

## 根本原因

HTTP/2 の読み込みバッファ（`read_buf` / `buf_start` / `buf_end`）への追記処理に 2 つの
潜在バグがあり、**B-07 の segfault がこれらを隠していた**（クラッシュが先に起きていたため
顕在化しなかった）。B-07 修正でプロセスが生存し続けるようになり露呈した。

### B-08a: `src/http2/client.rs` の `read_more`（致命的）

```rust
let read_slice = std::mem::take(&mut self.read_buf); // バッファ全体
let (result, returned_buf) = self.stream.read(read_slice).await; // RECV は先頭(offset 0)へ書く
self.read_buf = returned_buf;
match result { Ok(n) => { self.buf_end += n; ... } }
```

`IORING_OP_RECV` は `write_ptr`（=バッファ先頭）から書き込むため、バッファ全体を渡すと
**offset 0 から上書き**される。しかし `buf_end += n` は「`[buf_end..buf_end+n]` に追記された」
前提。未パースの部分フレーム（`buf_end > 0`）があると、その領域を破壊し、長さも誤る。

### B-08b: `src/http2/connection.rs` の `read_more`

`split_off(buf_end)` で読み込み先は正しいが、`full_buf.extend_from_slice(&returned_tail)` が
返却バッファの `len()` を信用している。`IoBufMut::set_init`（`src/runtime/buf.rs` の Vec 実装）は
**grow-only**（`if pos > len { set_len(pos) }`）のため、`compact_buffer` 後に残った末尾データ分
だけ `len > n` になり、残留バイトを取り込んでしまう。

## 改修

両 `read_more` を統一して以下にする。

1. `split_off(self.buf_end)` で **buf_end 以降の tail** を読み込み先に渡す（既存データを壊さない）。
2. 結合は `result` の **n バイトのみ**（`extend_from_slice(&returned_tail[..n])`）。返却 `len` は
   信用しない。
3. `Ok(0)` / `Err` 時は tail を捨てて `read_buf = full_buf`（不変条件 `read_buf.len() == buf_end`
   を維持）。

## 再現手順

```bash
TEST_FILTER=test_h2c ./tests/e2e_setup.sh test
# 修正前: "Frame too large" 多発、25/29 失敗
```

## 対応状況: 完了

`src/http2/client.rs` と `src/http2/connection.rs` の `read_more` を上記方針で修正。

### 検証

- 修正前: `TEST_FILTER=test_h2c` で 25/29 失敗（"Frame too large"）。
- 修正後: **H2C 29/29 通過**（"Frame too large" 消失、segfault なし）。

## 関連

- [B-07](B-07-iouring-future-drop-uaf.md): segfault 修正がこのバグを顕在化させた。
