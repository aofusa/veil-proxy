# B-240: 要求ごとの future が大きく、その move（`memcpy`）が静的配信の CPU の 3 割を占める

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の FreeBSD aarch64 の性能確認（2026-10-27）。v0.9.2 との交互 A/B（8 回、CPU/要求）で、静的配信の 3B が
`h2c_file_plain` +4.3%（8/8 回）・`h1_file_plain` +2.6%（7/8 回）。syscall の数は同じで、増えたのはユーザ空間。

## 調査

- 二分探索（`h2c_file_plain` 3B、8 回、閾値 1.025）で最初の悪化は F-272（`12ec9c54`、1.035）。ただし F-272 が要求ごとに
  足したのは原子変数の読み取り 1 回だけで、それを外しても 1.024 までしか戻らず、単独では説明できない。
- DTrace の `profile`（ユーザ空間）で、v0.9.2 にも HEAD にも libc の `memcpy` がサンプルの 28% あった（HTTP/2 の
  ストリームのタスクの起動元・executor の `spawn_body_and_poll` から）。
- `-Zprint-type-sizes`（macOS の `full-macos`）で、ストリームごとに作る `h2_request_task` の future が 11〜12.9KB。
  状態機械の大きさは一番大きい分岐で決まるので、静的配信の要求でもプロキシ・Wasm の分岐の大きさを持つ。
  - これを値で `TaskPool::spawn_inline` → `store` → スロットへ渡すたびに全体が `memcpy` される。
  - `.await` する子の future も親へ move される。静的配信の `h2_sendfile`（HTTP/2）・`handle_sendfile`・`handle_backend`
    （HTTP/1.1）は v0.9.2 の 3.1〜3.8KB から 5.9〜6.7KB に増えていた。中身は 5.1〜5.4KB の Wasm の future
    （`FilterEngine::on_*_with_modules_async`・`apply_wasm_request_body` 等。F-271 で大きくなった）で、Wasm を使わない
    ルートでも状態機械に埋め込まれている。

## 修正

- Wasm のコールバックの本体（`FilterEngine` の `*_async` の 5 つ、`apply_wasm_request_body`・`apply_wasm_response_body`、
  HTTP/3 の要求ヘッダ）を `Box::pin` で呼ぶ。呼び出し元の future から数 KB が消える。箱を確保するのは Wasm のモジュールが
  ある要求だけ（どの呼び出し元もモジュールが空なら呼ばないか、呼ぶ前にヘッダの `Vec` を作っている）。
- reactor の `TaskPool` に `spawn_inline_with`（future を作る閉包を受け取る）を足し、HTTP/2 のストリームのタスクは
  スロットの中で作る（作る関数を呼び出しのまま残して戻り値の領域をスロットにし、空きスロットへ `ptr::write` する）。
  io_uring の executor は変えない。
- 静的配信の子の future は `h2_sendfile` 6.0KB → 1.4KB、`handle_sendfile` 5.9KB → 2.1KB、`handle_backend` 6.7KB → 2.8KB
  （macOS の `full-macos` の `-Zprint-type-sizes`）。

HTTP/2 の上流への中継（`h2_proxy`、9.7KB）も `Box::pin` で呼ぶと、ストリームのタスクの future が 11.5KB → 3.7KB になり
静的配信はさらに速くなる（h2c 3B で CPU/要求 −21.6%・`memcpy` 9.8%）が、中継の要求が箱の確保と書き込みで遅くなる
（`h2_proxy_tls` 3B で、箱に入れない版に対して CPU/要求 +8%）ので入れない。中継の future そのもの（TLS のストリーム
1,088 バイトを状態ごとに持つ等）を小さくするのは別に扱う。

## 結果

FreeBSD 14.3 aarch64（QEMU + HVF）、修正前（`aba248fc`）との交互 A/B、3B、CPU/要求の比の中央値（`tools/perf/bsd` の
`cpu_per_req.sh` と同じ計り方で veil 側だけを 2 バイナリで）。

| シナリオ | CPU/要求 | rps | 回数 |
|---|---:|---:|---|
| `h2c_file_plain` | −11.0% | +12.4% | 8 回中 8 回で少ない |
| `h2_file_tls` | −13.4% | +15.6% | 8 回中 8 回で少ない |
| `h2_proxy_tls` | −5.6% | +6.0% | 12 回中 8 回で少ない |
| `h1_file_plain` | +1.8% | −1.9% | — |
| `h1_proxy_tls`（対照。コードは同じ） | −0.7% | +0.6% | — |

- `memcpy` のサンプルは 28.7% → 20.3%（`h2c_file_plain`、DTrace の `profile`）。
- HTTP/1.1 は `memcpy` が 1% しか無く、この修正で速くなる所が無い。`h1_file_plain` の差は配置の違いで、HTTP/1.1 のコードが
  同じ別のビルド（`h2_proxy` も箱に入れた版）では −0.9% だった。
- Windows（`full-windows`、`api` 込み）・macOS（`full-macos`）の clippy と単体試験、Linux の単体・E2E（両バックエンド）を通した。
