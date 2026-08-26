# F-165: ホットパスのアロケーション測定と最小化

## 目的

「1 リクエストあたりのヒープアロケーション回数」を **測定可能** にし、理論最小
（＝ゼロ、もしくは構造上不可避な数）へ近づける。AGENTS.md の「ホットパス絶対規則」
（アロケーション禁止・ゼロコピー徹底）を、レビューではなく**実測**で担保する。

## Phase 1: 測定手段（`alloc-stats` feature）

既定ビルドに影響を与えない **オプトイン feature `alloc-stats`** を追加する。

- `src/alloc_stats.rs`: 内側アロケータ（mimalloc / jemalloc / System）を包む
  `CountingAllocator`。`alloc` / `dealloc` / `realloc` の回数とバイト数を
  `AtomicU64`（`Relaxed`）で数える。
- `veil::alloc_stats::snapshot()` でカウンタを取得、`reset()` でゼロクリア。
- 既定 feature には**入れない**（`default` 不変）。有効時のみ `#[global_allocator]` が
  カウンティング版に差し替わる。無効時はコード自体がコンパイルされない＝ゼロコスト。
- 負荷試験での使い方: `reset()` → h2load `-n N` → `snapshot()` を
  `N` で割って「1 リクエストあたりのアロケーション数」を出す。
  ワーカー起動・設定ロードなどのコールドパス分は warmup 後に `reset()` することで除外する。
  取得は Prometheus メトリクス（`metrics` feature 有効時）と
  `SIGUSR2`/定期ログではなく、**`[prometheus]` の `veil_alloc_*` 系ゲージ**として公開する
  （負荷中に外から読めることが重要）。

補助手段:

- `tools/perf/h2c_proxy_lab.sh strace` による 1 リクエストあたり syscall 数（既存）。
- `docker` 内 `valgrind --tool=dhat`（ホストに valgrind が無いため）で
  アロケーション**呼び出し元**の特定を行う。

## Phase 2: 削減対象（コードレビューで先行検出した候補）

| # | 箇所 | 内容 | 見積 |
|---|---|---|---|
| A1 | `http2/client.rs::send_request` | ヘッダ名が既に小文字でも `name.to_vec()` している（`lowered_names`）。小文字化が必要なものだけコピーする | ヘッダ数分の malloc |
| A2 | `proxy.rs::h2_proxy_h2c` | `H2C_POOL.put(addr.to_string(), ..)` がリクエストごとに `String` を確保 | 1/req |
| A3 | `proxy.rs::h2_proxy_h2c` | 応答ヘッダを `(k.clone(), v.clone())` で全ディープコピー | ヘッダ数×2/req |
| A4 | `http2/client.rs::receive_response` | `H2cResponse` の `headers`/`body`/`trailers` が `Vec<Vec<u8>>` | ヘッダ数+2/req |
| A5 | `runtime/reactor/poller.rs::FdRecord` | `read_wakers`/`write_wakers` が `Vec<Waker>`。待機・起床のたびに 1 malloc + 1 free | I/O 待機ごと |
| A6 | HPACK encode / フレームエンコード | 出力先が都度 `Vec` | フレームごと |

理論最小値の考え方: 「接続あたり 1 回で済むもの」「設定ロード時に解決できるもの」
「プール/`Bytes` の共有で済むもの」を除いた残りが理論下限。**リクエストあたり 0 を目標**とし、
達成できない項目は理由を記録する。

## Phase 3: 実測 → 改修 → 再実測

交互 A/B（`tools/perf/h2c_proxy_lab.sh ab`）で 3B（固定費支配）と 54KB（バイト単価支配）の
両方を測る（F-159 の教訓）。

## Phase 3: 実測に基づく第 2 ラウンド（2026-08-26）

`alloc-stats` + `tools/perf/alloc_measure.sh` による **実測値**（`full-container`、
h2load `-n 20000 -c 100 -m 10`、コミット 63565e8 時点）:

| 構成 | allocs/req | bytes/req | reallocs/req |
|---|---|---|---|
| `h2c_proxy` 54KB | 50.7 | 61,455 | 3.33 |
| `h2c_proxy` 3B | 54.7 | 11,134 | 3.31 |
| `h2c_file` 3B（静的） | 33.4 | 3,201 | 2.31 |
| `h2c_file` 54KB（静的） | 32.4 | 3,424 | 3.31 |

**読み方**: 静的配信でも 1 リクエスト 32〜33 回確保している＝**HTTP/2 サーバ側の固定費**が
支配項。プロキシはそこへ +18〜22 回。54KB プロキシの 61KB/req は
**ボディチャンクの `Bytes::copy_from_slice`**（`h2_stream_body_cl`）がそのまま出ている。

### 第 2 ラウンドの対象

| # | 対象 | 内容 | 見積 |
|---|---|---|---|
| R1 | `http2/hpack/decoder.rs` + `table.rs` | `HeaderField` の `name`/`value` を `Vec<u8>` から `Bytes` へ。デコーダが持つ **アリーナ `BytesMut`** へ書いて `split_to().freeze()` で配る。静的テーブルは `Bytes::from_static`（確保ゼロ）、動的テーブルは参照カウント clone | **-2/ヘッダ ≈ -16〜24/req** |
| R2 | `proxy.rs::H2RequestCtx` | `method`/`path`/`authority` を R1 のアリーナ由来 `Bytes` に（現状は per-stream の `Vec<u8>` コピー 3 個） | -3/req |
| R3 | `http2/hpack/encoder.rs` 出力 | エンコード先を都度 `Vec` からプール／再利用バッファへ | -1〜2/req |
| R4 | `proxy.rs::h2_stream_body_cl` ほか | ボディチャンクの `Bytes::copy_from_slice` をプール `BytesMut` の `split_to().freeze()` へ（**F-157 の教訓によりコピー削減は必ず A/B で確認する**） | bytes/req の大半 |
| R5 | `runtime/reactor/tcp/unix.rs` | 「直前に EAGAIN を観測した」ことが分かっている待機では、park 前の確認用 `poll(2)` を省略して直接 register する（ET なら新規到着が必ずエッジを生むため安全）。**epoll のみ**（F-158 の教訓によりバックエンド間で一般化しない） | syscall -0.5/req |


### R5 は見送り（負の結果、2026-08-26）

「EAGAIN 直後の確認用 `poll(2)` を省略する」実装は API とテストまで書いたうえで**撤回した**。

- 実際に `poll(2)` を出しているのは `ReadFuture`/`WriteFuture` **ではない**（これらは
  try-first の `read(2)`/`write(2)` で EAGAIN を観測したらそのまま `register_read` するので
  `poll(2)` を通らない）。出しているのは **`h2_select_readable_or_notify` が使う
  `wait_readable_fd`**（HTTP/2 接続ループ）など、「EAGAIN 直後ではない」待機である。
- そこでの `poll(2)` は**有用な仕事をしている**: まだ park していない時点では
  `epoll_wait` を通っていないためヒントが立っておらず、`poll(2)` が「もう届いている
  データ」を検出して park を丸ごと省く。これを省略すると
  「register → Pending → park → `epoll_wait`（即時復帰）→ wake」に置き換わるだけで、
  syscall は減らずレイテンシが増える可能性が高い。
- したがって前提条件（「直前に同じ fd で EAGAIN を観測した」）を満たす呼び出し箇所が
  ホットパスに存在せず、API だけが残る（＝デッドコード）ため実装を差し戻した。

**教訓**: syscall プロファイルの数値（`poll` 0.53/req）を見て「この待機経路だろう」と
当たりを付ける前に、**どのコード経路がその syscall を出しているかを特定する**こと。
