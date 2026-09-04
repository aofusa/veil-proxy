# B-85: グローバルキャッシュを触る単体テストが並列実行で干渉し、docker ビルドが非決定に失敗する

## 事象

`cargo test --lib`（`--features full`）を回すと、
`cache::content_cache::tests::mtime_revalidation_reload_also_invalidates_compressed_cache`
が**低頻度で失敗**する。失敗箇所は `src/cache/content_cache.rs` の
`assert_eq!(hit_before, first);`（「TTL が残っている間は圧縮結果キャッシュがヒットする
はず」という assert）。

**これは単なる「たまに落ちるテスト」ではない。** `docker/Dockerfile.glibc` の
builder ステージが `cargo test --lib` をビルドの一部として実行するため、**この
フレーキーさがそのまま `veil:base` イメージのビルドの非決定的な失敗になる**
（実際に `test result: FAILED. 954 passed; 1 failed` でイメージビルドが落ちた事例を
確認済み）。影響は単体テストに閉じず、packaging・perf 計測・E2E のコンテナ経路
すべてに波及する。

## 原因

テスト間で共有される**グローバル状態**への並列アクセスが競合する。

- 圧縮結果キャッシュは `src/cache/compressed.rs` の
  `static COMPRESSED_CACHE: Lazy<CompressedCache>` で、**プロセス全体で 1 つ**。
- 本体キャッシュも `src/cache/content_cache.rs` の `static CONTENT_CACHE` で同様に
  グローバル。
- `ContentCache::clear()` は `super::compressed::clear()` を連鎖呼び出しする
  （本体キャッシュのクリアが圧縮結果キャッシュも道連れにする設計。これ自体は
  意図通りで正しい）。
- `cache::content_cache::tests::public_api_smoke_test` はこのグローバルな
  `clear()` を 2 回呼ぶ。

cargo はテストをデフォルトで並列スレッド実行するため、`public_api_smoke_test` の
`clear()` が、`mtime_revalidation_reload_also_invalidates_compressed_cache` の
2 回の `get_or_compress` 呼び出しの**間**に割り込むと、キャッシュエントリが消えて
圧縮クロージャが再実行され、「TTL 内なのにヒットしない」という失敗になる。

同様に、グローバル `COMPRESSED_CACHE`/`CONTENT_CACHE` を直接触るテストは
`src/cache/content_cache.rs` と `src/cache/compressed.rs` の両方に存在し、互いに
干渉しうる。一方、`ContentCache::new()` / `CompressedCache::new()` で**テスト
ローカルなインスタンス**しか触らないテスト（大多数）は元々干渉しない。

## 影響

- `cargo test --lib` がローカル・CI で低頻度に失敗する。
- **`docker/Dockerfile.glibc` の builder ステージのビルドが非決定的に失敗する**
  （本質はこちら）。packaging・perf 計測・E2E のいずれもこのベースイメージに
  依存するため、無関係な変更のビルドが理由不明に落ちて見える。

## 対処（2026-09-04）

グローバル `CONTENT_CACHE`/`COMPRESSED_CACHE` を**グローバル API 経由**で触る
テストにのみ `#[serial_test::serial]` を付け、直列実行を強制した。`#[serial]`
は同じグループ名（引数なし＝既定グループ）を共有するテスト同士だけを直列化
するため、`content_cache.rs` と `compressed.rs` の両方を**同じ既定グループ**に
入れ、モジュールをまたいで直列化されるようにした（両者が同じ
`COMPRESSED_CACHE` を共有するため、別グループに分けると意味がない）。

対象（各 `#[serial]` にコメントで理由を明記済み）:

- `src/cache/content_cache.rs`
  - `invalidate_also_drops_compressed_cache_variants`
    （`super::super::compressed::get_or_compress` 経由でグローバル
    `COMPRESSED_CACHE` を触る）
  - `mtime_revalidation_reload_also_invalidates_compressed_cache`（同上）
  - `public_api_smoke_test`（グローバル `clear()`/`invalidate()`/`len()`/
    `get_or_load()` を直接呼ぶ）
- `src/cache/compressed.rs`
  - `public_api_get_or_compress_smoke_test`（グローバル `get_or_compress`/
    `invalidate` を直接呼ぶ）

`ContentCache::new()`/`CompressedCache::new()` のテストローカルインスタンスしか
触らないその他のテスト（`hit_and_miss`、`ttl_expiry_forces_reload`、
`hit_avoids_recompression`、`invalidate_drops_all_variants_for_path` 等）は
グローバル状態と無縁なので `#[serial]` を付けていない。

`serial_test = "3.5.0"` は既に `[dev-dependencies]`（`Cargo.toml`）にあり、
`tests/e2e_tests.rs` の統合テストで使用済みだったため、追加の依存追加は不要
だった。`[dev-dependencies]` はライブラリの `#[cfg(test)]` からも参照できる。

本番コード（`src/cache/` の非テストコード、キャッシュの実装・`clear()` の連鎖
呼び出し設計）は変更していない。テストの分離のみが問題であり、キャッシュの
挙動自体は正しい。

### 検証

- `cargo fmt` / `cargo clippy --features full --all-targets -- -D warnings`
  警告ゼロ。
- `cargo test --lib --features full` を 10 回連続実行し、10 回とも成功を確認。
- `cargo build --no-default-features` が通ることを確認。
