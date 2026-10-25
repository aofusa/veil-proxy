# F-270: API ライブラリのハンドラの応答に `[route.compression]` を効かせる（HTTP/1.1・HTTP/2・HTTP/3、バッファ・ストリーミング）

**状態: 未着手（2026-10-25 起票・設計。親は [F-258](F-258-v0100-plan.md)）**

## 背景（調査で確認）

- `Backend::Handler(entry, security, modules)` に圧縮の設定の欄が無い（`src/config/backend.rs` 83〜87 行。`Proxy` だけが持つ 22〜30 行）。
- HTTP/1.1: ルートの解決で圧縮の設定を捨てる（`src/proxy/h1.rs` 776 行 `_route_compression`）。`handle_api_handler`（1511〜1588 行）は
  `dispatch::write_http1_head` と生の本文、ストリーミングは `write_api_chunked`（1611 行）で、`compress.rs` を通らない。
- HTTP/2: `route_compression` は Proxy と File の分岐だけ（`h2.rs` 1515〜1578 行）。`h2_api_respond` は素通し。
- HTTP/3: `h3_api_respond`（`request_task.rs` 174 行）も同じ。`compress_body_h3` は Proxy と File だけ。
- ライブラリで応答を圧縮できるのは gRPC（`[api] grpc_response_compression`）だけ。
- HttpArena の json-comp（framework で採点対象）は `Accept-Encoding: gzip, br` に対して `Content-Encoding: gzip|br` を求め、
  standard モードは **フレームワークの組み込みの圧縮** だけを許し（手書きの圧縮は禁止）、点は応答の小ささの 2 乗で伸びる。
  `Accept-Encoding` が無ければ `Content-Encoding` を付けてはいけない。

## 制約

- ライブラリ利用時だけの分岐（`cfg(feature = "handler")`）。本体のプロキシ・File の圧縮の経路は変えない。
- ホットパス: 圧縮しない応答（設定が無い・小さい・`Accept-Encoding` 無し・既に `Content-Encoding` がある）の経路に確保を足さない。
  圧縮の作業領域は本体と同じスレッドローカルのエンコーダのプール（`compress.rs` の仕組み）を使う。

## 設計

### 1. 設定

- ハンドラのルートでも `[route.compression]`（`enabled`・`algorithms`（`br`・`gzip`・`zstd`・`deflate`）・`level`・`min_size`・
  `content_types`）をそのまま読む。設定ロード時に `Backend::Handler` へ `Arc<CompressionConfig>`（`None` = 無効）を解決して持たせる
  （F-148/F-159 の「派生値はロード時に解決」）。
- ライブラリの既定: `[api] compression`（新設、ライブラリ専用。既定 `false`）を `true` にすると、全ハンドラのルートの既定の
  `[route.compression]` を `{ algorithms = ["br", "gzip"], min_size = 256, content_types = ["application/json", "text/*", "application/javascript", "image/svg+xml"] }` にする
  （ルートごとの `[route.compression]` が優先）。
- API: Rust の `App::compression(Compression::default())`・`#[veil::route(..., compress = false)]`（ルートで止める）、
  `Response::no_compression()`（応答ごとに止める）。Python の `veil.run(compression=True)`・`@app.get(..., compress=False)`・
  `veil.Response(..., compress=False)`。C の `veil_config_set(cfg, "api.compression", "true")`。

### 2. 経路

- **バッファ応答**（ほとんどの JSON）: ハンドラの応答の本文（`Bytes`）を、`Accept-Encoding` の q 値と設定の順で選んだ方式で 1 回で圧縮し、
  `Content-Encoding`・`Vary: Accept-Encoding` を付け、`Content-Length` を圧縮後の長さにする。HTTP/1.1・2・3 で同じ関数を呼ぶ
  （`src/api/dispatch.rs` に置き、3 経路は呼ぶだけにする）。`ETag` があれば弱い ETag（`W/`）にする。
- **ストリーミング応答**（`Response::stream`・SSE・Python のジェネレータ）: ストリームのエンコーダで断片ごとに圧縮して流す。
  SSE（`text/event-stream`）は既定で圧縮しない（断片ごとの flush が要り、プロキシ・ブラウザの扱いが揃わない。設定で有効にできる）。
- 圧縮しない条件: 本文が `min_size` 未満、`Content-Type` が対象外、`Content-Encoding` が既にある、状態が 204・304・1xx、HEAD、
  `Cache-Control: no-transform`、`Range` の応答（ハンドラの 206）。
- 品質と速度: 既定の `level` は br 4・gzip 6（本体の `[route.compression]` の既定と同じにする）。HttpArena の json-comp は小ささが効くので、
  提出物ではレベルを上げるかを計測で決める（[F-269](F-269-benchmark-submissions.md)。standard の「本番ガイドの推奨設定」の範囲で。
  [F-274](F-274-library-production-guide.md) に推奨値を書く）。

## 試験

- 単体: `Accept-Encoding` の解析（q=0・`*`・`identity;q=0`）、条件ごとに圧縮しない、`Vary`・`Content-Length`・弱い ETag。
- E2E（`library_e2e.sh`、Rust・Python・C、HTTP/1.1・h2c・HTTP/2・HTTP/3）: br・gzip を解凍して元と一致、`Accept-Encoding` 無しで
  `Content-Encoding` が無い、ストリーミングの圧縮、`compress = false`。HttpArena の json-comp の validate と同じ確認。
- 非退行: `nonregression`、本体の圧縮の E2E。
- 性能（`tools/api-bench`）: 圧縮を有効にしないアプリの `/json` が変わらない。`/json`（圧縮あり）を axum + tower-http の
  `CompressionLayer`・FastAPI + `GZipMiddleware` と比べる。

## 文書

- `docs/guide/library.md`・`compression-cache.md`（英日）、`crates/veil-python/docs/configuration.md`（英日）、`examples/config.toml` の `[api]`。

## 受け入れ条件

- 3 プロトコルでハンドラの応答が圧縮され、本体のバイナリと圧縮しない応答の性能が変わらない。文書（英日）・`backlog.md` が更新されている。
