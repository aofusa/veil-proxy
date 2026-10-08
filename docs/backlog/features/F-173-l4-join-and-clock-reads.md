# F-173: L4 の 2 方向転送で空打ち read を消す・HTTP/3 の時刻読み取りを減らす

## L4: `futures::join!` → `FuturesUnordered`

FreeBSD aarch64 の `l4_tcp`（3B）で `read` が 1 リクエストあたり 6.0 回（nginx の `recvfrom`
は 3.0 回）。L4 は c→u / u→c の 2 方向を `futures::join!` で同時に回していたが、`join!` は
**片方が起床するたびに両方を poll する**（子ごとの Waker を持たない）。reactor の
`read`/`readable` は poll されるたびにまず `read(2)`/`poll(2)` を試すので、起床していない側が
毎回 EAGAIN を空打ちしていた。

起床した子だけを poll する `FuturesUnordered` に置き換えた（`l4::proxy::join_directions`。
子タスクのノード確保は接続あたり 2 回で、リクエストごとには発生しない）。
`read` 6.0 → **4.0/req**、合計 syscall 8.23 → **6.26/req**（nginx 8.12）。

## HTTP/3: 時刻読み取り

FreeBSD aarch64 の h3_file CPU プロファイル（DTrace）で `__vdso_gettc` が最上位だった。

- `schedule_timer` はダーティ接続ごと・イテレーションごとに `Instant::now() + conn.timeout()`
  を計算していたが、quiche の `timeout()` は内部でも `Instant::now()` を読むため 1 回あたり
  2 回だった。`conn.timeout_instant()`（期限の絶対時刻）をそのまま使い、`None` のときだけ読む。
- リクエストごとの開始時刻は、アクセスログ・メトリクスがすべて無効なら使われないので
  ループ時刻キャッシュ（`runtime::time::coarse_now`）で済ませる
  （`logging::request_start_instant`、判定は `logging::access_outputs_active`）。
