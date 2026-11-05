# F-294: 静的配信で事前圧縮した兄弟ファイル（`.br`・`.gz`・`.zst`）を返す（nginx の `gzip_static`・`brotli_static` 相当）

**状態: 実装済み（2026-11-05。全 OS の E2E・A/B は v0.11.0 の試験で）**

調査の全文は `docs/artifacts/v0110/review-06-precompressed.md`（git 管理外）の §1（今の経路）・§3（設計）。行番号は f8dc80de 時点。

## 今の状態

- 事前圧縮した兄弟は使わない（`docs/releases/v0.10.1.md:118` の Known limitations、`library-kit.md:96`・`library-production.md:129`（英日））。
- 静的配信の応答は 3 プロトコルとも `src/static_response.rs` の `plan()`（If-None-Match → If-Modified-Since → 圧縮するか（Range より優先）→ Range/If-Range）と
  `emit_headers` を通る。圧縮はその場で行い（HTTP/1.1 はファイルを読んで `cache::compressed::get_or_compress`、HTTP/2・HTTP/3 は全体をメモリへ読む）、
  圧縮すると sendfile・kTLS のゼロコピーを捨てる。
- 交渉・ETag・キャッシュに既存の不具合がある（[B-302](../bugs/B-302-accept-encoding-negotiation.md)〜[B-305](../bugs/B-305-h1-static-open-beneath-wrong-root.md)）。
  兄弟の交渉は B-302 の直した解析器、表現ごとの ETag は B-304 の構造の上に作るので、**B-302・B-304 を先に直す**。

## 改修内容（設計）

### 設定

```toml
[route.action]
type = "File"
path = "/srv/www/assets/"
precompressed = ["br", "gzip"]   # 既定 []（無効）。"zstd" も可。並びは同点のときの優先順
```

- `[route.action]` に置き、`[route.compression]` と独立に効く（圧縮を有効にしなくても使え、**kTLS・sendfile のゼロコピーのまま圧縮した表現を返せる**）。
  拡張子は固定（`br`→`.br`、`gzip`→`.gz`、`zstd`→`.zst`）。`deflate`・重複・未知はロード時のエラー。ルートの派生値はロード時に解決する（F-148/F-159）。
- 兄弟は**元のファイルがあるときだけ**探す（nginx と同じ）。`/app.js.br` を直接要求されたら普通のファイルとして返す。

### 交渉

- `Accept-Encoding` を B-302 の確保しない解析器で読む（q=0 は不可、`*` は列挙されていない方式、`identity` は暗黙に受理）。
- 候補 = 兄弟がある方式 ∪ `[route.compression]` でその場で作れる方式。q の最大、同点は兄弟ありを優先 → `precompressed` の並び。無ければ identity。

### 応答（表現を先に決めてから条件付き要求・Range を評価する）

- `Repr::{Identity, Precompressed(enc), OnTheFly(enc)}`。兄弟の応答: `Content-Type` は元の MIME、`Content-Length` は兄弟の大きさ、`ETag` は強い
  `"<兄弟の mtime>-<size>-br"`、`Last-Modified` は元、`Accept-Ranges: bytes`、`Vary: Accept-Encoding`（304・206・416 にも）。
- Range は圧縮した表現のバイト範囲（RFC 9110 §14.1.2）。If-Range は兄弟の強い ETag と比べる。HEAD は stat だけ。

### 探索（ホットパスの外）

- 兄弟の状態（`Absent` / `Present{共有 fd, size, 検証子}`）を**元のファイルの `open_file_cache` のエントリ**に持たせ、エントリを作る offload の中で続けて開く。
  ヒット時は syscall・確保ゼロ（配列の添字）。無い兄弟も同じ寿命で覚える（ネガティブキャッシュ）。TTL・F-272 の `watch` の世代で元と一緒に取り直す。
  `skip_types` の型（画像等）では探さない。`precompressed` があって `open_file_cache` が無効なルートは起動時に警告する。
- 封じ込め: **ルート自身の dirfd** に対する `openat2(RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS)`（F-153/F-154）。`EXDEV`/`ELOOP` の従来経路と FreeBSD の
  capsicum（`O_RESOLVE_BENEATH`）以外の canonicalize 経路では、兄弟の正規パスにも `sendfile_base_contains` をかける（`app.js.br -> /etc/shadow` を拒否）。
  `canonicalize()` はホットパスで使わない。
- 検証: `O_NONBLOCK` で開いて（FIFO で offload が詰まらない）`S_ISREG` だけ。**元より古い兄弟は使わない**（作り忘れで古い内容を返さない。stat 済みの値の比較だけ）。

### 送出

- HTTP/1.1: `handle_sendfile` の `(file, size, 検証子)` を兄弟のものに差し替えるだけで、平文・kTLS・rustls・FreeBSD の `sf_hdtr` がそのまま使える。
- HTTP/2・HTTP/3: 静的コンテンツキャッシュに兄弟のパスをキーにして持つ（FreeBSD の h2c の共有メモリ + sendfile もそのまま効く）。
- `mode = "memory"`: ロード時に兄弟も読む（ETag は内容のハッシュ + 方式）。リロード（F-178 の capsicum 下の経路）でも読み直す。

### ライブラリ

- `ServeOptions.precompressed`（`App::serve_dir`）、`veil-kit` の `serve`、Python の `app.serve_dir`・`veil_kit.StaticFiles(precompressed=...)`、C は
  [F-291](F-291-capi-helpers-kit-parity.md) の `veil_config_serve_dir`。`ServeOptions` は `#[non_exhaustive]` でない公開構造体なので、フィールド追加は
  構造体リテラルの利用者を壊す → `#[non_exhaustive]` を付けるか、リリースノートで `..Default::default()` を案内する。

### 文書

- `routing.md`・`compression-cache.md`・`library-kit.md`・`library-production.md`（英日）、`examples/config.toml`、Known limitations の行を消す。
  あわせて [B-297](../bugs/B-297-zero-copy-compression-docs-mismatch.md) の D1〜D8。

## 受け入れ条件

- 単体: 交渉の表（q=0・`*`・同点・兄弟の有無 × その場の圧縮の有無）、方式ごとの ETag、304、`.br` への Range・If-Range、シンボリックリンクの脱出・絶対リンク、
  ディレクトリ・FIFO・古い兄弟を無視、キャッシュのヒットで offload ゼロ。
- E2E: HTTP/1.1（平文・kTLS・rustls）・h2c・HTTP/2・HTTP/3（quinn に加えて h2load でも）、io_uring・epoll、FreeBSD の VM。
- 交互 A/B: `precompressed` の無いルートで 3B・54KB の性能・allocs/req が変わらない。ある場合は兄弟の応答がゼロコピー（strace で sendfile）。

## 修正

- `[route.action] precompressed = ["br", "gzip", "zstd"]`（`config::Precompressed`。未知・重複はロード時のエラー）。`Backend::SendFile` の 10 番目の値。
- 兄弟の探索は `src/cache/precompressed.rs`: 元のファイルのパスごとに兄弟の有無の表（DashMap。open_file_cache の有効期間・最大数・監視の世代。
  元のファイルの大きさ・更新時刻が変わったら探し直す）。兄弟は `get_file_info_with_config`（open_file_cache・封じ込めの高速経路・フォールバックをそのまま）で開き、
  通常のファイル・ルートの封じ込め（ディレクトリのルート）・元のファイル以上の更新時刻を確かめる。open_file_cache が無効なら毎回探す。
- 選び方 `static_response::precompressed_candidates`: q の降順（同点は設定の順）、その場の圧縮の q の方が厳密に高い方式は除く。型では絞らない（nginx の gzip_static と同じ）。
- 検証子 `Validators::for_sibling`（強い ETag `"<mtime>-<size>-br"`・元の Last-Modified・元の型）が `content_encoding` を持ち、`emit_headers` が 200・206 に
  `Content-Encoding` を付ける。兄弟は `no_compression()` で `plan` するので Range・304 はそのまま効く。ルートに `precompressed` があれば `Vary` を常に付ける。
- HTTP/1.1 は兄弟の fd・大きさで `sendfile`（kTLS も）。HTTP/2・HTTP/3 は `select_with_content` で本体を静的コンテンツキャッシュから。memory は
  ロード時に兄弟を読む（`MemoryContent::load_precompressed`。FIFO で止まらないよう `O_NONBLOCK`、FreeBSD の capability mode は dirfd 相対）。
- ライブラリ: `ServeOptions::precompressed`、Python の `app.serve_dir(precompressed=...)`・`StaticFiles(precompressed=...)`。C は F-291 の `veil_config_serve_dir` 側。
- 文書: routing・compression-cache・library-kit・library-production（英日）、`examples/config.toml`。
- 試験: 単体（候補の順・その場の圧縮との比較・兄弟の検証子のヘッダ・memory の選択と Vary）、E2E `test_f294_precompressed_http{1,2,3}`（古い兄弟を無視・304・Range）。
- `ServeOptions` は `#[non_exhaustive]` にせず、リリースノートで `..Default::default()` を案内する。

## 関連

- [F-286](F-286-range-requests-h2-h3-static.md)、[F-287](F-287-static-etag-conditional-requests.md)、[B-269](../bugs/B-269-h1-static-files-not-compressed.md)、
  [F-169](F-169-static-compressed-variant-cache.md)（圧縮結果のキャッシュ）、[B-302](../bugs/B-302-accept-encoding-negotiation.md)〜[B-305](../bugs/B-305-h1-static-open-beneath-wrong-root.md)
