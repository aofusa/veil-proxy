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
