# B-406: WAF の例のフィルタ（と E2E のフィクスチャ）が要求ごとに規則の正規表現をコンパイルしていた。今の例は起動できなかった

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

## 事象

B-404・B-405 を確かめる E2E（seccomp の `log`、io_uring）で `coverage::test_http3_wasm_local_response` が 403 ではなく 503 を
返した（594 件中 1 件。同じ条件で 3 回回し直すとすべて通った）。B-110 で直したはずの負荷の高いときの失敗だった。

調べると 2 つの問題があった。

1. 追跡しているフィクスチャ `tests/fixtures/wasm/waf_filter.wasm`（と手元の `tests/wasm/waf_filter.wasm`。`e2e_setup.sh` は
   こちらを先に使う）は、例 `examples/wasm-filters/waf-filter` の最初の版（2025-12-26、fd9ede98）からビルドしたもので、
   `on_http_request_headers` が規則ごとに `Regex::new` を呼んでいた。1 要求に 50〜58 ms のゲストの CPU を使い、負荷の高い
   ときに要求のコールバックの実時間の締切（約 1 秒）を超えてトラップし、`fail_closed` の 503 になった。
2. 今の例（069ff58c 以降。CRS Level 2 の 30 規則）は、規則を HTTP コンテキストごとに複製するとき（`impl Clone for CompiledRule`）に
   正規表現をコンパイルし直していた。また Unicode の正規表現（`.{1,100}` 等が UTF-8 の巨大なオートマトンになる）の組み立てが
   起動時の CPU の予算（`max_execution_time_ms`、既定 100 ms）に収まらず、veil の起動時の検査（B-301）で「wasm module
   'waf_filter' failed to start ... all fuel consumed」と止まった。今の例をそのまま使った利用者の設定は起動しなかった。

## 修正

- 例: 規則は `on_configure` で 1 つの `RegexSet` へまとめてコンパイルし（`regex::bytes`、Unicode を切った ASCII の照合）、
  ルートコンテキストが `Rc` で持って HTTP コンテキストと共有する（複製でコンパイルしない）。照合は 1 回の走査で当たった規則を
  規則の順に返す（従来の結果と同じ）。カスタム規則は 1 つずつ検証して、コンパイルできないものは飛ばす（従来どおり）。
  `WafFilterRoot::new` で既定の規則を組み立ててから `on_configure` で組み立て直す二度手間もやめた。
- 例をビルドし直して両方のフィクスチャを置き換えた（起動時の予算 50 ms でも起動する。`instance_reuse = true` で 1 要求 2〜3 ms）。
- 例の README（と `examples/wasm-filters/` の他の例の README）の設定例が、存在しない `[[routes.wasm_filters]]` の書き方だった
  ので、`[wasm]`・`[[wasm.modules]]`・`configuration`・`[wasm.modules.capabilities]`・ルートの `modules` に直した。WAF の README に
  `instance_reuse = true` を勧める理由と ASCII の照合を書いた。
- E2E `coverage::test_http3_wasm_local_response`: スキャナの User-Agent（`sqlmap`、`crs-913100`）と、`:path` のクエリの
  パストラバーサルの 2 つで 403 を確かめる（後者は B-408 で `:path` が読めるようになって初めて発火する）。以前の試験は古い版にしか
  無い規則（`curl` の User-Agent）に頼っていた。

## 調べる途中で見つけたもの

- [B-407](B-407-wasm-aot-cache-mtime-staleness.md): AOT キャッシュを mtime だけで新しいと判断していた（`cp -p` で古い版を戻すと
  新しい版のネイティブのコードを読む。E2E の準備がまさにそうしていた）。
- [B-408](B-408-proxy-wasm-pseudo-headers-and-request-properties.md): ゲストから `:path` 等の疑似ヘッダが読めなかった。
- [B-409](B-409-config-test-skips-tls-and-wasm-startup.md): `veil -t` が起動で止まるモジュールを見つけなかった。
- [B-410](B-410-waf-example-whitelist-bypass.md): 例の WAF の許可リストを素通りできた。
- [B-411](B-411-wasm-allowed-properties-docs-default.md): `allowed_properties` の既定を文書が逆に書いていた。

## 試験

例の `cargo test --locked`（12 件）。手元の veil で起動時の予算 100 ms・50 ms の起動、`instance_reuse` の有無ごとに通常の要求
200・`sqlmap` 403・クエリのパストラバーサル 403 を確かめた。E2E は io_uring・epoll で回す。
