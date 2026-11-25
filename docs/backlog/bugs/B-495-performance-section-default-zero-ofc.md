# B-495: `[performance]` を書かない設定で open_file_cache の有効期間・最大件数が 0 になり、ルートの open_file_cache が一度も当たらない

**状態: 完了（2026-11-25、feat/httparena-perf。2026-11-22 起票。F-317 の HttpArena の調査で見つけた。v0.10.1・main とも同じ）**

## 対応（2026-11-25）

- `src/config/mod.rs` に `serde_defaults<T>()` を足した（空の map を serde で読むので、各フィールドの `#[serde(default = "...")]` が効く）。
  `PerformanceConfigSection` と `GlobalSecurityConfig`（`src/config/compression.rs`）の `Default` をこれで書き、derive の `Default` を外した。
  節が無いときと、節が 1 行だけのときの値が同じになる（open_file_cache は 60 秒・10000 件）。
- `file_cache`: `max_entries == 0` は「何もキャッシュしない」（入れない）。`evict_oldest` は上限まで確実に減らす（以前は `max_entries / 10` 件で、
  上限が 10 未満だと 0 件しか消さず、上限を超えて増え続けた）。文書（英日の configuration.md・`examples/config.toml`）に 0 の意味と、
  `[performance]` を書かない設定でも既定が同じことを書いた。
- 挿入のたびの `entries.len()` はミスのときだけ通る（キャッシュに当たるようになったので要求ごとの経路には無い）。要求ごとの共有の書き込みは F-321 で扱う。
- 試験: `config::section_default_tests`（`[performance]`・`[security]` が無いときの値が serde の既定と一致。derive の `Default` に戻すと落ちる）、
  `cache::file_cache::tests::test_insert_with_config_small_and_zero_limits`（上限 0 は入れない。上限 5 で 20 件入れても 5 件以下で、最新が残る）。
- 計測（aofish、HttpArena のエントリと同じ設定）は [F-317](../features/F-317-httparena-infra-score-plan.md) の「計測の記録」。

## 事象

- `PerformanceConfigSection`（`src/config/mod.rs`）は `#[derive(Deserialize, Clone, Default)]` で、設定の側は
  `#[serde(default)] performance: PerformanceConfigSection`。`[performance]` の節が**無い**と serde は derive の `Default` を使い、
  フィールドの `#[serde(default = "...")]` は効かない。
  - `open_file_cache_valid_duration_secs` = **0**（文書の既定は 60）
  - `open_file_cache_max_entries` = **0**（文書の既定は 10000）
  - `manual_chunk_size` = 0（`chunk_size_mode = "manual"` は節が要るので実害は無い）
  - 節があれば（中身が 1 行でも）各フィールドの既定（60・10000）になる。
- `apply_global_cache_settings` がこの 0 を全体の既定（`OPEN_FILE_CACHE_GLOBAL_VALID_DURATION_NANOS`・`..._MAX_ENTRIES`）へ書く。
  `[route.open_file_cache]` で `valid_duration_secs`・`max_entries` を書かないルートは全体の既定を継ぐので、
  - `peek_with_config` の `is_valid(0)` が常に偽 → **エントリが一度も使われない**（要求ごとに `openat2`・`statx`・本体の `preadv2`）。
  - `insert_with_config` が毎回 `len >= 0` で `evict_oldest()` を呼ぶ。`evict_oldest` は全エントリのキーを `Vec` へ複製して
    並べ替え、`max_entries / 10 = 0` 件を消す（何も消さない）。**要求ごとに DashMap の全シャードの走査と確保**が入る。
  - 静的コンテンツキャッシュ（`static_file_cache`）の引き当ては open_file_cache が当たったときだけ行う（F-157）ので、本体の
    キャッシュも使われない。HTTP/2・HTTP/3 はさらに B-496 で要求ごとに圧縮し直す。
- 再現（ノート PC、4 CPU、main の io_uring ビルド。`docs/artifacts/benchmark/httparena-perf/`）: HttpArena の veil エントリと同じ設定
  （`[performance]` 無し、`/static/*` の File ルートに `[route.open_file_cache] enabled = true`）で、
  - strace: 12 要求（h1 6・h2c 6）で `openat2(…"vendor.js"…)` が 12 回（キャッシュに当たれば 1 回）。
  - `[performance]` に `open_file_cache_enabled = true` を足すだけで、h2c の静的配信（br）は 1 要求 18〜24 ms → 0.8〜0.9 ms。

## 影響

- HttpArena の veil エントリ（`[performance]` 無し）の static-tls・static-h2・static-h3 が 97k・14k・14k rps（nginx 721k・800k・361k）。
  CPU は 11〜19 コアしか使っていない（veil-api・nginx は 64 コア）。要求ごとの `evict_oldest`・DashMap のロック・本体の読み直し
  （`RWF_NOWAIT` が `EAGAIN` なら offload）で並列に進まないと見ている（64 コアでの確認は F-320 の計測で行う）。
- 一般の利用者も同じ: `[performance]` を書かずにルートで open_file_cache を有効にした設定は、キャッシュが効かないまま動く
  （エラーも警告も出ない）。

## 改修案

1. `PerformanceConfigSection` の `Default` を手書きし、serde の既定と同じ関数（`default_open_file_cache_valid_duration` 等）を使う。
   同じ形（derive の `Default` + `#[serde(default = "...")]`）の型をもう 1 つ見つけた: `GlobalSecurityConfig`
   （`src/config/compression.rs`。`sandbox_no_new_privs`・`sandbox_mount_proc` 等が節の無いときに false）。有効化のフラグも同じ節に
   あるので今は実害が無さそうだが、同じ直し方にそろえる。
2. 回帰試験: 対象の型すべてで「空の TOML から読んだ値 == `Default::default()`」を確かめる単体試験（derive の `Default` に戻すと落ちる）。
   設定のロードの試験で、`[performance]` 無し + ルートの open_file_cache で 2 回目の `peek_with_config` が当たること。
3. `evict_oldest` は `max_entries` が 0 のとき何も消さずに全走査する。`max_entries == 0` を「無効」と扱うか、0 を設定の検証で
   弾く（`examples/config.toml` と文書に書く）。ついでに挿入の度の `self.entries.len()`（全シャードの読み取りロック）を
   カウンタにする（`content_cache.rs` の `try_insert` も同じ。F-318 の「共有の書き込み」と同じ観点）。

## 受け入れ条件

- `[performance]` 無しで、ルートの `[route.open_file_cache] enabled = true` のファイルの 2 回目以降の要求が `openat2` を呼ばない（strace か
  キャッシュのヒット数）。
- 上の単体試験。E2E（静的配信）・`cargo test`。
- 文書の既定値（60 秒・10000）と実際の値が一致する。

## 関連

- [F-317](../features/F-317-httparena-infra-score-plan.md)（親）・[B-496](B-496-content-cache-reinsert-drops-compressed.md)・
  [F-320](../features/F-320-httparena-veil-entry-config.md)（設定での回避）・F-157・F-272
