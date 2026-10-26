# F-264: Swagger UI を完全にオフラインで使えるようにする（資産をライブラリに同梱）

**状態: 完了（wt/core → feat/v0100、2026-10-25。2026-10-25 起票・設計。親は [F-258](F-258-v0100-plan.md)）**

## 背景

`/docs`（`[api] openapi_path`）の HTML は `swagger-ui-dist@5` の CSS・JS を **unpkg の CDN** から読む
（`src/api/openapi.rs` `swagger_ui_html`）。版も `@5` で固定していない。

- 閉じたネットワーク・社内環境・機内では `/docs` が真っ白になる（`docs/guide/library.md` の Limitations にも書いてある）。
- 第三者の CDN のスクリプトを利用者のオリジンで実行する（SRI も無い）。CSP の `script-src 'self'` と両立しない。
- Python（`veil-api`）の `/docs` も同じ Rust の経路で配る。
- 同じ問題が GraphQL のサンプル（`examples/api/rust-graphql` の GraphiQL、`python-graphql`）にもある（[F-267](F-267-rust-kit-crate.md)・
  [F-268](F-268-python-kit-fastapi-compat.md) の GraphQL で同じ方法で直す）。

## 制約

- ライブラリ利用時だけ（`openapi` feature）。本体（`full` 等）のバイナリには資産を 1 バイトも入れない
  （`nonregression` に「`swagger-ui` の文字列が full バイナリに無い」検査を足す）。
- 外部への通信を 0 にする（HTML・CSS・JS・フォント・画像・source map の参照を含めて）。

## 設計

### 1. 資産の同梱

- `third_party/swagger-ui-dist/<版>/` に `swagger-ui-bundle.js`・`swagger-ui.css`・`LICENSE`（Apache-2.0）・`NOTICE`・
  取得元と sha256 を書いた `README.veil.md` を置く（`favicon` は同梱しない、または SVG 1 個）。版は 5.x の最新の安定版に固定する。
  - `swagger-ui-standalone-preset.js` は使わない（トップバーの URL 入力は外部の定義を読むためのもので不要）。
  - バンドルの末尾の `//# sourceMappingURL=` は取り除く（ブラウザの開発者ツールが外へ取りに行かないように。取り除いたことを README に書く）。
- ビルド時に **gzip と brotli で事前圧縮** したものだけを `include_bytes!` で持つ（`build.rs` で作るのではなく、更新の台本が作って
  リポジトリに入れる。ビルドの依存と時間を増やさない）。生の JS は持たない（約 1.4 MB → 圧縮後 約 350 KB / 300 KB）。
  `Accept-Encoding` が gzip も br も受けないクライアントには、起動時に 1 回だけ解凍した `Bytes` を遅延生成して返す
  （`OnceLock`。最初の要求だけ解凍の費用。`compression` feature の flate2 を使い、無いビルドでは gzip の展開だけ小さな実装を使う）。
- 配信: `/docs/swagger-ui-bundle.js`・`/docs/swagger-ui.css`（`openapi_path` の下）を組み込みのハンドラで返す。
  `Content-Type`・`Content-Encoding`・`Vary: Accept-Encoding`・`ETag`（版 + sha256）・`Cache-Control: public, max-age=31536000, immutable`
  （URL に版を含める: `/docs/assets/5.x.y/swagger-ui-bundle.js`）。本文は `Bytes::from_static`（複製しない）。
- HTML: 同じオリジンの資産を読む形にし、`Content-Security-Policy: default-src 'none'; script-src 'self' 'sha256-<初期化スクリプトのハッシュ>'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; font-src 'self' data:` を付ける。
  初期化スクリプトのハッシュは HTML を作るとき（起動・リロード時）に計算する（タイトル・JSON の URL を埋め込むため）。
- `[api] openapi_docs_ui`（新設）: `"embedded"`（既定）・`"cdn"`（今の挙動。資産を持ちたくない人向けに `cdn_url` を指定可、
  既定は版を固定した jsDelivr）・`"off"`（`/openapi.json` だけ）。Cargo feature `openapi-ui-embedded`（`openapi` に既定で含める）を
  外せば資産を持たないビルドもできる（その場合 `"embedded"` は設定のエラー）。

### 2. 更新の手順

- `tools/release/update-swagger-ui.sh <版>`: npm のレジストリから tarball を取り、npm の `integrity`（sha512）で検査し、
  2 ファイルを取り出し、source map の参照を外し、gz・br を作り、`README.veil.md` の版・sha256 を更新する。
- 脆弱性の検査: `grype`・Trivy はバイナリに埋めた JS を見ないので、`tools/container_security` の依存の検査に
  「`third_party/swagger-ui-dist` の版を npm の advisory（`npm audit` 相当の API）で照合する」項目を足す（AGENTS.md の
  「直す版の無いものも失敗」の方針に合わせる）。
- `cargo-deny` の licenses に Apache-2.0 の第三者資産として記録する（crate ではないので `README.veil.md` と `NOTICE` で管理）。

### 3. Python

- `veil-api` の wheel は同じ Rust の拡張なので、追加の作業は無い（wheel が約 0.7 MB 増える）。`veil.run(openapi_docs_ui="embedded")`
  でも選べるようにする。

## 試験

- 単体: HTML に外部の URL（`http://`・`https://`・`//`）が 1 個も無い、CSP のハッシュが初期化スクリプトと一致、
  資産の sha256 が `README.veil.md` と一致、`Accept-Encoding` ごとの応答（br・gzip・無し）と `ETag` / `If-None-Match` の 304。
- E2E: ネットワークを切ったコンテナ（`--network none` + ループバック）で Rust・Python のアプリを起動し、ヘッドレスの Chromium
  （Playwright。`tools/` の試験用だけ）で `/docs` を開いてエラー 0・外部への要求 0・操作一覧が描画されることを確かめる。
- 非退行: `nonregression` に `swagger-ui` の文字列の検査。`--no-default-features --features handler,openapi`（`openapi-ui-embedded` 無し）がビルドできる。

## 文書

- `docs/guide/library.md`（英日）: `[api] openapi_docs_ui` の表、Limitations の CDN の行を削除、CSP の説明。
- `crates/veil-python/docs/configuration.md`（英日）。`examples/config.toml` の `[api]`。

## 受け入れ条件

- ネットワークの無い環境で `/docs` が完全に動く（Rust・Python）。本体のバイナリに資産が入らない。
- 文書（英日）・`backlog.md` が更新されている。

## 実施内容（2026-10-25）

- **資産**: `third_party/swagger-ui-dist/5.34.0/`（npm の latest）に `swagger-ui-bundle.js`・`swagger-ui.css` の gzip（level 9）と
  brotli（quality 11）だけ（生のファイルは置かない。計 約 850 KB）と `LICENSE`・`NOTICE`。`README.veil.md` に取得元・npm の
  integrity・sha256・取り除いたもの（末尾の `sourceMappingURL`）。`assets.rs`（台本が作る定数と `include_bytes!`）を
  `src/api/swagger_ui.rs` が `include!` する。crates.io のパッケージに入るよう `Cargo.toml` の `include` に足した。
- **台本**: `tools/release/update-swagger-ui.sh <版>`（npm の integrity（sha512）を検査・source map の参照を外す・gz/br・
  README と assets.rs を作り直す。node の zlib・crypto を使う）、`--check`（gz と br が同じ中身・sha256 が README と assets.rs
  に一致・source map の参照が無い）、`--audit`（npm の bulk advisory API。該当があれば直す版の有無にかかわらず失敗）。
  `--audit` は `tools/container_security/security/run_cargo_audit.sh`（release.yml も回す）の先頭で回す（判定マーカーの前）。
- **配信**: `src/api/swagger_ui.rs`（新設。`openapi.rs` の変更は `/docs` のハンドラが CSP 等のヘッダを付けるのと、
  資産のルートの登録・ページの作成をこのモジュールへ回す数行だけ）。`<openapi_path>/assets/<版>/swagger-ui-bundle.js`・
  `swagger-ui.css`・`swagger-initializer.js` を組み込みのハンドラ（`__veil_openapi_asset_*`。`openapi_enabled = false` で
  消える）で返す。`Accept-Encoding` で br → gzip → 無し（最初の要求で 1 回だけ flate2 で解凍した `Bytes` を `OnceLock` に
  持つ）、`Vary: accept-encoding`・`ETag`（版 + sha256 の先頭 16 桁）・`If-None-Match` で 304・
  `Cache-Control: public, max-age=31536000, immutable`・`nosniff`。本文は `Bytes::from_static`、ETag も `Bytes`（要求ごとの
  確保なし）。
- **ページ**: 同じオリジンの資産だけを読む HTML に `Content-Security-Policy: default-src 'none'; script-src 'self';
  style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:; connect-src 'self'; base-uri 'none';
  form-action 'self'; frame-ancestors 'self'`・`nosniff`・`Referrer-Policy: no-referrer`。`validatorUrl: null`
  （validator.swagger.io へのバッジの要求を止める）。
- **設定**: `[api] openapi_docs_ui`（`"embedded"`（既定）・`"cdn"`・`"off"`）と `[api] openapi_docs_cdn_url`（既定は版を固定した
  jsDelivr）。`Config::openapi_docs_ui`・`openapi_docs_cdn_url` のセッタ。Python は `veil.run(openapi_docs_ui=...)`。
  Cargo feature `openapi-ui-embedded`（`openapi` + flate2）を `api-core`（したがって `api`）・`veil-capi`・`veil-python` に入れた。
  無いビルドでは既定が `"cdn"` で、`"embedded"` は設定のエラー（`refresh` が返す）。
- **試験**: 単体 `api::swagger_ui::tests`（設定の解釈、ページに外部の URL・インラインのスクリプトが無い・CSP、同梱の版と
  CDN の既定の版が同じ、`Accept-Encoding` の解釈、資産ごとに br・gzip・無しの応答と解凍した中身の sha256 が README と一致・
  source map の参照が無い・ETag で 304）。`tests/api/library_e2e.sh rust` に `/docs` の外部参照 0・CSP・br・gzip・無し（解凍）・
  h2c・304 の確認、`nonregression` に full（と Linux の full-container）のバイナリに `swagger-ui`・`SwaggerUIBundle` の文字列が
  無いことと、`--features handler,openapi`（`openapi-ui-embedded` 無し）がビルドできること。
  `tests/api/swagger_offline.sh`（新設）: ネットワークの無いコンテナ（`docker run --network none`）の中の Playwright の Chromium で
  Rust（`rust-hello`）・Python（`python-hello`、このリポジトリの wheel）の `/docs` を開き、コンソールのエラー（CSP の違反を含む）0・
  他のオリジンへの要求 0・操作が描画されることを確かめる。`examples-nightly.yml` の job `swagger-offline` で毎晩回す。
  イメージ（`mcr.microsoft.com/playwright/python`）はブラウザだけで Python の playwright を持たないので、ホストで wheel を落として
  渡す。docker の既定の seccomp は io_uring を拒否するので `--security-opt seccomp=unconfined` で起こす。

- F-262 の WebSocket の操作（`x-veil-websocket`）で Try it out を出さないプラグイン（`VeilNoTryItOut`）は、同梱の初期化の
  スクリプトと CDN のページの両方に入れた（`swagger_ui.rs` の `no_try_it_out_plugin!`）。`swagger_offline.sh` は
  `rust-websocket` で `/ws`・`/typed` に無く `/plaintext` に有ることも見る。

### 検証

feat/v0100（F-261〜F-263 を含む）へ載せ直した後（a23b2681、Windows の python は 721d4a8d）で:

- Linux（手元）: `cargo fmt --check`、clippy `-D warnings`（`full --all-targets`・`full,epoll --all-targets`・`--lib --tests` の
  `api`・`--lib` の `handler,openapi`）、`cargo test --lib --bins --test integration_tests --features full`（1149 + 63 件）、
  `library_e2e.sh nonregression`（full・full-container のバイナリに `swagger-ui` 無し、`handler,openapi` がビルドできる）・`rust`・
  `python` が ALL OK、`swagger_offline.sh all` ALL OK（Rust の hello・websocket と Python が描画、CSP 違反・コンソールの
  エラー・他オリジンへの要求 0、`/ws`・`/typed` に Try it out が無く `/plaintext` に有る。`script-src 'self'` のままで
  `unsafe-eval` は要らない）、`check-messages.py`・`check-docs.sh`・`update-swagger-ui.sh --check` が通る。
- macOS（ssh llm）: clippy `-D warnings`（`--lib` の `handler,openapi`・`--lib --tests` の `api`・`full-macos,api`）、単体、
  `library_e2e.sh rust`・`nonregression`・`python` が ALL OK。
- Windows（ssh aofish）: clippy `--lib --tests` の `full-windows,api`、単体、`rust`（2 回）・`nonregression`・`python` が ALL OK。
  `python` は F-263 の常駐ループ（Windows の既定の ProactorEventLoop に `add_reader` が無い）と `ws_check.py`（`resource` が
  無い。feat/v0100 の b5bc84d6 と同じ直し）を直してから通った。

### 決めたこと

- 初期化のスクリプトをインラインに置かず、同じオリジンの静的なファイル（`swagger-initializer.js`）にした。OpenAPI の JSON の
  URL は `<div id="swagger-ui" data-url="...">` から読む。CSP は `script-src 'self'` だけで済み、起動・リロード時のハッシュの
  計算（sha256・base64 の依存）が要らない。
- 圧縮を受けないクライアントへの解凍は flate2 だけ（`openapi-ui-embedded` が flate2 を引く。小さな gzip の展開の自前実装は作らない）。
- 資産の URL（`/docs/assets/<版>/...`）は `openapi_path` の下。資産のルートの登録は最初の 1 回だけ（`/docs` のルートと同じ。
  リロードで `openapi_path` を変えても資産の URL は変わらない、今までの `/docs` と同じ制約）。
- cargo-deny には記録しない（crate ではない）。`README.veil.md`・`LICENSE`・`NOTICE` で管理する。
- GraphiQL（F-267・F-268）は対象外（それぞれのチケットで同じ方法で直す）。

