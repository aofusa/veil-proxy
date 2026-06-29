# B-09: L4 forward_direction が読み取りバイト数でなくバッファ全長を送信する

## 事象

L4 TCP プロキシ（`tls = "passthrough"`）でクライアントの TLS ハンドシェイクが完了しない。
proxy ログに以下が出る（c→u は転送されるが u→c が 0）。

```
[L4:l4-passthrough] 127.0.0.1:xxxxx → 127.0.0.1:9001 (tls=Passthrough)
[L4:l4-passthrough] connection closed: c→u 1442 bytes, u→c 0 bytes
```

新規追加した L4 E2E（`test_l4_tcp_passthrough_forward` / `test_l4_passthrough_large_payload`）で
発覚。**L4 には E2E が無かったため長らく未検出だった**（F-30 で E2E を追加して顕在化）。

## 影響

L4 ストリームプロキシの **すべての転送**（各方向の最初の読み取りチャンク）で、実際に読み取った
バイト数ではなくバッファ全長（64KB）を送信していた。生 TCP 中継では致命的で、TLS パススルー時は
バックエンドが破損した ClientHello を受け取りハンドシェイクを拒否、レスポンス（u→c）が返らない。

## 根本原因

`src/l4/proxy.rs` の `forward_direction`:

1. `Vec::with_capacity(BUF_SIZE)` + `set_len(BUF_SIZE)` で **len=64KB** のバッファを read に渡す。
2. `ReadFuture<Vec<u8>>` 完了時の `IoBufMut::set_init`（`runtime/buf.rs` の Vec 実装）は
   **grow-only**（`if pos > len { set_len(pos) }`）。read 後も `b.len()` は 64KB のまま n に縮まらない。
3. `dst.write(b)` は `IoBuf::bytes_init() == b.len() == 64KB` を送信。実 n バイト（例: ClientHello
   1442）+ **未初期化の 64KB 末尾**を送ってしまい、転送データが破損する。
   （`total += n` で n を加算するためログ上は 1442 と表示され、実送信量との乖離も見えにくかった。）

## 改修案

read 完了後、実際に読み取った n バイトへバッファ長を切り詰めてから write する。

```rust
let n = match res { Ok(0) | Err(_) => break, Ok(n) => n };
// set_init は grow-only なので明示的に n へ切り詰める
unsafe { b.set_len(n) };
// 以降の write は n バイトのみを送信する
```

## 検証

- 修正前: `TEST_FILTER=test_l4 ./tests/e2e_setup.sh test` で 2/2 失敗（u→c 0 バイト、15s タイムアウト）。
- 修正後: L4 E2E 通過。

## 関連

- F-30（L4 splice ゼロコピー化）の前提として L4 E2E を追加した際に発覚。splice 化後はカーネル内
  転送のため本クラスのバグは構造的に発生しない。
