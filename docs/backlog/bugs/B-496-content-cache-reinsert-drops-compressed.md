# B-496: 静的コンテンツキャッシュへ同じ版の本体を入れ直すと圧縮結果のキャッシュを捨て、HTTP/2・HTTP/3 の静的配信が要求ごとに圧縮し直す

**状態: 未着手（2026-11-22 起票。F-317 の HttpArena の調査で見つけた。v0.10.1・main とも同じ）**

## 事象

- HTTP/2・HTTP/3 の静的配信（`h2_sendfile`・HTTP/3 の `request_task`）は `cache::get_static_file_with_content` で本体を得て、
  `static_response::build_bytes_response` → `cache::compressed::get_or_compress` で圧縮結果をキャッシュする（F-169）。
- `get_static_file_with_content` は open_file_cache が当たらないと（B-495・有効期限切れ・ルートで無効）高速経路でファイルを読み、
  **毎回** `content_cache::insert_bytes_stamped` で本体を入れ直す。`ContentCache::try_insert` は既存のエントリがあると
  （`Entry::Occupied`）本体を置き換え、**版が同じでも無条件に** `compressed::invalidate(path)` を呼ぶ（`content_cache.rs` の
  F-169 のコメント「古い内容に対する圧縮結果を必ず道連れにする」）。
- その結果、open_file_cache が当たらない間は HTTP/2・HTTP/3 の要求ごとに圧縮結果が消え、**br・gzip で毎回圧縮し直す**。
  HTTP/1.1（`proxy/sendfile.rs`）は本体を別の経路（`load_file_bytes`）で読み、入れ直さないので圧縮結果に当たる。
- 再現（ノート PC、main、`docs/artifacts/benchmark/httparena-perf/`）: HttpArena の veil エントリと同じ設定（B-495 で open_file_cache が
  当たらない）で、`curl --http2-prior-knowledge -H 'Accept-Encoding: br'` の 300 KB の JS が毎回 9〜27 ms（HTTP/1.1 は 2 回目から 0.6 ms）。
  veil-load の h2c（16 接続 × 32 ストリーム、HttpArena の 20 ファイル）で 710 rps・1 要求 1,588 µs の CPU。
  open_file_cache を当たるようにする（`[performance] open_file_cache_enabled = true`）と 43,216 rps・26 µs。

## 影響

- HttpArena の static-h2・static-h3 が 14k rps（1 要求 1.2〜1.4 ms の CPU。nginx は 800k・361k、veil-api は 1M・289k）。
- 一般の利用者: open_file_cache を無効にしたルート、有効期限が切れた直後、`[performance]` の無い設定（B-495）で、HTTP/2・HTTP/3 の
  圧縮が効く静的ファイルの要求が圧縮の CPU を毎回払う（br の既定の品質だと 300 KB で 10 ms 前後）。

## 改修案

1. `ContentCache::try_insert` の `Occupied` で、置き換える本体の版（`CachedContent::version`、B-349 の `FileVersion`）が既存と同じなら
   圧縮結果を残す（本体も置き換えない。必要なら `cached_at`・`stamp` だけ更新する）。版が違うときだけ今までどおり `invalidate`。
   `version` を持たない古い経路（`mtime: Some(..)` の入れ方）は今の挙動のまま。
2. 圧縮結果のキー（`file_ident(size, mtime_secs)`）は秒単位の mtime と大きさなので、版が同じかの判定は本体の側（`FileVersion`）で行う
   （圧縮結果だけで判定すると、1 秒以内の同じ大きさの書き換えを見逃す）。
3. 回帰試験: open_file_cache が無効のルートで、同じファイルを HTTP/2 で 2 回取ると 2 回目は圧縮しない（`compressed::misses()` が増えない）。
   ファイルを書き換えると圧縮し直す（版が変わる）ことも確かめる（F-169・F-272 の試験を壊さない）。

## 受け入れ条件

- open_file_cache が当たらない構成でも、HTTP/2・HTTP/3 の静的配信の 2 回目以降が圧縮結果のキャッシュに当たる。
- 書き換えたファイルは新しい内容で圧縮し直す（版が変わったとき）。E2E（静的配信・圧縮）・`cargo test`。

## 関連

- [F-317](../features/F-317-httparena-infra-score-plan.md)（親）・[B-495](B-495-performance-section-default-zero-ofc.md)・F-169・F-157・B-349
