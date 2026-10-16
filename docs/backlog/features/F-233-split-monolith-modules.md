# F-233: 巨大ファイルの分割（proxy.rs・config.rs・http3_server.rs・tests/e2e_tests.rs）

**状態: 未着手（2026-10-16 起票・設計。出典: docs/artifacts/veil-comprehensive-review-and-comparison.md の §4.2 ①・§5 Phase 2 の 4）**

## 目的

1 ファイルに多くのプロトコルと機能が同居しているので、ファイルの境界を機能ごとに整理する。中身（関数のシグネチャ・
本体・インライン化の属性）は変えず、移すだけにする。F-234（L7 の共通化）の前に行い、F-234 の差分を読める大きさにする。

## 今の大きさ（2026-10-16 の main）

| ファイル | 行数 | 関数 |
|---|---:|---:|
| `src/proxy.rs` | 13,352 | 120 |
| `src/config.rs` | 10,097 | 153 |
| `src/http3_server.rs` | 6,961 | 60 |
| `tests/e2e_tests.rs` | 26,037 | 557 |

## 設計

### `src/proxy.rs` → `src/proxy/`

今のファイルの中の区切り（`// ====` の見出し）が、そのまま分け目になる。

| 新しいファイル | 今の区切り（行は 2026-10-16 の main） |
|---|---|
| `proxy/mod.rs` | 接続処理（52〜）・公開する入口・`pub(crate) use` の再エクスポート |
| `proxy/security.rs` | 共通セキュリティチェック（94〜。HTTP/1.1・2・3 で共用） |
| `proxy/h2.rs`・`proxy/h2_stream_body.rs` | HTTP/2 ハンドラー（331〜）・F-32 の要求方向のストリーミング（4527〜） |
| `proxy/detect.rs` | プロトコル検出（H2C。4903〜） |
| `proxy/h1.rs` | 要求の処理ループ（5358〜）・リダイレクト（7059〜） |
| `proxy/websocket.rs` | WebSocket（7142〜） |
| `proxy/forward.rs` | プロキシ処理・HTTP プロキシ（プール）・H2C プロキシ（7714〜9163） |
| `proxy/buffered.rs`・`proxy/compress.rs` | バッファリングの結果型・送受信（9164〜）・圧縮付きの送受信と転送（9966〜） |
| `proxy/splice.rs` | kTLS + splice(2) の高速経路（10884〜） |

- 外から見えるパス（`crate::proxy::xxx`）は、`mod.rs` の再エクスポートで変えない。`src/mesh/`・`src/http3_stream.rs`・
  `src/api/`・テストからの参照を書き換えずに済ませる。
- 分割後も同じクレートなので、意味は変わらない。ただし rustc はコード生成単位（CGU）をモジュール単位で割り当てる。
  `[profile.release]` は cargo 既定の codegen-units=16・LTO 無しなので、別の CGU に分かれた関数同士は、`#[inline]` が無いと
  インライン化されなくなる。そこで、ホットパスの関数を移すときは、次のようにする。
  - 対象: `handle_requests`・`handle_proxy`・`drive_h2_streams`・プロキシの送受信。
  - それらが呼ぶ小さな関数（今は同じファイルにあるもの）を一覧にする。
  - `suite_ab.sh` で差が出たら、その関数にだけ `#[inline]` を足す。足す理由と計測をコミットに書く。
  - 配布の `[profile.dist]` は fat LTO なので、この影響を受けない。release と dist の両方で測る。
- `cfg(feature)` は、モジュール単位で付けられるものは `mod` 宣言の 1 箇所に寄せる（例: `#[cfg(feature = "http2")] mod h2;`）。
  これで報告の §4.2 ⑥（cfg の散らばり）も一部減る。

### `src/config.rs` → `src/config/`

- `config/mod.rs`: 設定のルートの型・読み込み・検証。
- `config/route.rs`: `Route`・`Backend`・`load_backend`。
- `config/tls.rs`・`config/http2.rs`・`config/http3.rs`・`config/upstream.rs`・`config/security.rs`: 各セクションの serde の型と
  既定値の関数。
- レートリミッタ（`check_rate_limit`）や、LB・ヘルスチェックの実行時のロジックは設定の型ではないので、
  `src/routing/`・`src/resilience.rs` へ移す。
- `examples/config.toml` との同期（AGENTS.md）は、セクションと同じ名前のファイルにすることで追いやすくする。

### `src/http3_server.rs` → `src/http3/`

`http3/server.rs`（メインループ・ダーティ集合）・`http3/conn.rs`・`http3/static.rs`・`http3/forward.rs` に分ける。
今の `http3_stream.rs`・`http3_wire.rs` も、この下へ移す（`http3/stream.rs`・`http3/wire.rs`）。

### `tests/e2e_tests.rs` → 機能ごとのファイル

- `tests/e2e/` の下に、`main.rs`（共通のヘルパー・サーバの起動）と、機能ごとのモジュール（`h1.rs`・`h2.rs`・`h3.rs`・`grpc.rs`・
  `wasm.rs`・`tls.rs`・`security.rs`・`static.rs`・`cache.rs`・`vds.rs` 等）を置く。テストのバイナリは 1 つのままにする
  （`[[test]] name = "e2e_tests" path = "tests/e2e/main.rs"`）。
  - 1 つのままにするのは、`tests/e2e_setup.sh` のサーバの起動を共有するためである。
  - テスト名は変えない。`tests/e2e_setup.sh` の絞り込み・CI の記録・`docs/guide/testing.md` が名前で参照しているため。
- 増分ビルドの短縮は、1 つのバイナリの中でもモジュールが分かれていれば、rustc の増分コンパイルで効く。分割の前後で、
  1 つのテストを書き換えたときの再ビルドの時間を測って記録する。

## 進め方

ファイル 1 つにつき 1 コミットにし、ほかの変更を混ぜない（`git log --follow` と `git blame -C` で履歴を追えるようにする）。

- 各コミットで `cargo build`（全 feature の組み合わせ）・clippy・`cargo test` を通す。
- proxy.rs と http3_server.rs を分けたコミットでは、交互 A/B も行う。
- 移す前と後で、`cargo +nightly rustc -- -Zunpretty=expanded` の差分を見る。並べ替え以外の差分が無いことを確かめる。
- 並行して進む機能の作業（F-226・F-229 など）と衝突しやすいので、分割は短い期間で一気に行う。その間に main に入った変更は、
  移した先のファイルで当て直す。

## 受け入れ条件

- （必須）既存のプロキシの性能が退行しない（`tools/perf/suite_ab.sh` の main との交互 A/B、比の中央値 0.98 以上・0.95 未満なし。
  両バックエンド、3B と 54KB、h2・h3 を含む）。
- 公開 API（`veil::` の下）が変わらない（`cargo public-api` の差分が空）。
- ファイルの上限の目安を守る: 1 ファイル 3,000 行以下。超えるファイルは、理由を `mod.rs` の冒頭に書く。
- `cargo test`・E2E（io_uring・epoll）・BSD・macOS・Windows のクロスビルドが通る。
- AGENTS.md の「ディレクトリ要約」を更新する。

## 関連

F-232、F-234、AGENTS.md の行動指針 4（大きなロジックは専用モジュールへ）。
