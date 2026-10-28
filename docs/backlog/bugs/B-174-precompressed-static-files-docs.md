# B-174: 静的配信の文書が「事前圧縮した `.br`・`.gz` を配る」と書いている（File バックエンドは使わない）・Python の TypeError の文が日本語

**状態: 修正済み（v0.10.0）**

## 出典

F-269（HttpArena の提出物の準備）の validate で、`/static/*` に `Accept-Encoding: br` を付けても `Content-Encoding` が
付かず（`SKIP [static-tls compression]`）、`app.js.br` が隣にあっても 204800 バイトの元のファイルが返った（2026-10-26）。

## 事象

- File バックエンド（`type = "File"`）は事前圧縮した兄弟のファイル（`.br`・`.gz`。nginx の `gzip_static`）を選ばない。
  `src/` に該当する処理は無い。
- それなのに次の文書が「precompressed files work as for the proxy」と書いていた（F-267・F-268・F-274 で書いた）:
  `src/api/app.rs`（`App::serve_dir` の rustdoc）、`crates/veil-kit/src/serve.rs`、`docs/guide/library-kit.md`（英日）、
  `docs/guide/library-production.md`（英日）、`crates/veil-python-kit/src/veil_kit/staticfiles.py`、`crates/veil-python/docs/kit.md`。
- 実際の挙動（2026-10-26 に `[route.compression] enabled = true` の File ルートで確認）: HTTP/2 は `content-encoding: br`
  （204800 → 57588 バイト、伸長すると元と一致）、HTTP/1.1 は圧縮しない（sendfile でそのまま送る）。HTTP/3 は
  `request_task.rs` の File の経路で `resolve_http3_compression_config` を使う（圧縮する）。
- 同じ調べで、`crates/veil-python/python/veil/__init__.py` の `_toml_literal` の `TypeError` の文が日本語だった
  （利用者に見える文は英語にする規則。`serve_dir(..., **route_options)` に TOML にできない値を渡すと出る）。

## 修正

- 上の文書から「事前圧縮ファイル」を外し、「事前圧縮した `.br`・`.gz` は使わない。`compression`（`[route.compression]`）は
  HTTP/2・HTTP/3 でその場で圧縮する（HTTP/1.1 はそのまま送る）」と書いた。
- `TypeError` の文を英語（`cannot convert to TOML: ...`）にした。
- 事前圧縮ファイルを選ぶ機能そのもの（と HTTP/1.1 の File の圧縮）は本チケットでは足さない（機能の候補。F-269 の
  `findings.md` に記録）。

## 試験

- `crates/veil-python/tests/test_binding.py` の `TestRouteOptions`（`route_options` の TOML 変換と、変換できない値の
  英語の `TypeError`）。
- 文書の記述は File ルートへ `curl`（HTTP/1.1・`--http2-prior-knowledge`、`Accept-Encoding: br;q=1, gzip;q=0.8`）で確認した
  （上の事象の数値）。`tools/release/check-docs.sh --python`・`tools/lint/check-messages.py`。
