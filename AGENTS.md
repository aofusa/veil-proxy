# AGENTS.md — Veil (veil-proxy)

AI エージェントおよびコントリビュータ向けの **最小指針**。機能説明・ビルド例・テスト手順の **正** は [README.md](README.md) / [docs/readme/README.ja.md](docs/readme/README.ja.md)。フィーチャー定義は [Cargo.toml](Cargo.toml) の `[features]`。

---

## プロジェクトの事実

- クレート名 `veil`（ディレクトリ名 `veil-proxy`）。リリースバイナリは `target/release/veil`。
- **ライブラリ + バイナリ構成**。mod 宣言・公開 API は [src/lib.rs](src/lib.rs)（`cargo fuzz`・統合テスト向けに公開）、サーバ起動配線は [src/entry.rs](src/entry.rs)（`veil::run()`）、[src/main.rs](src/main.rs) は `veil::run()` を呼ぶだけの薄いエントリ。
- 設定は TOML（`serde`）。ホットリロード・検証の挙動は README を参照。

---

## 設計哲学・こだわりポイント

**Rust の安全性を土台に、Linux カーネル（io_uring、kTLS、seccomp、Landlock、ソケット/CBPF）とユーザー空間を噛み合わせ、HTTP/1.1・2・3 のデータプレーンを tokio/monoio なしで高スループット化・ゼロコピー化。io_uring は `src/runtime/` の独自実装（libc + bytes クレートのみ使用）を通じて直接操作する。開発効率を度外視して最大限の性能を目指し、かつ運用で効く動的設定・観測・拡張（Proxy-Wasm）まで載せる。**

変更やレビューでは、個別機能だけでなく **ここに反しないか** を意識する。

---

## ホットパス絶対規則（最優先・例外なし）

データプレーン（接続受理〜リクエスト/レスポンス転送〜TLS/HTTP/2/HTTP/3/WASM 実行〜バックエンド I/O の各経路。1 リクエスト/1 コネクションあたり実行される全コード）では、次を **例外なく** 守る。レビュー時もこの観点を最優先で確認する。

- **同期処理（ブロッキング呼び出し）の使用を一切禁止する。** ホットパスのあらゆる I/O・待機・システムコールは **必ず非同期**（`src/runtime/` の io_uring 非同期 API、`.await`）で行う。`std::net`・ブロッキング `libc::read/write/connect`・同期 DNS 解決・`block_on`・`std::thread::sleep`・同期ロック待ち等をホットパスに置いてはならない。WASM 実行のような CPU バウンド処理も、ワーカースレッドを占有しないよう非同期（協調的 yield）で実行する。対応する io_uring オペコードが存在しないブロッキング処理（例: シンボリックリンク解決を伴う `canonicalize`）は `src/runtime/offload.rs` の `offload()`（専用スレッドプール + スレッドごと eventfd の POLL_ADD で完了待機）でワーカースレッドへ退避し、**イベントループ自体は決してブロックしない**こと（新規 io_uring オペコードを増やしてセキュリティサーフェスを広げてはならない）。
- **メモリアロケーションは、パフォーマンス上必要である場合を除いて一切禁止する。** リクエストごとの `Vec`/`String`/`HashMap`/`Box` 等の新規確保、`to_vec()`/`to_string()`/`clone()`（ディープコピー）/`format!`/`collect()` をホットパスで増やさない。
- **ゼロコピーを徹底する。** バッファは `bytes` クレート（`Bytes`/`BytesMut`、参照カウントによる共有・`split()`/`freeze()` によるゼロコピー分割）、`src/pool.rs` のスレッドローカルバッファプール、`splice(2)`/`sendfile(2)` 等のカーネルゼロコピー機構を用い、アロケーションとコピーを発生させない実装にする。
- **難易度や保守性を理由に妥協しない。** 実装・設計の難易度が高い場合でも一切妥協せず、保守性や実装難易度は度外視して、**最高性能のパフォーマンスとセキュリティ** を最優先に設計・実装する。
- 既存コードを変更・追加する際は「これはホットパスか？」を常に自問し、ホットパスなら上記をすべて満たすこと。満たせない設計なら設計自体をやり直す。

## 設計制約（要約）

変更時は **上記の設計哲学・ホットパス絶対規則** および次の箇条書きに反しないか確認する。

- **データプレーンは tokio / monoio に依存しない**（テスト・クライアント用途の tokio は別）。io_uring 非同期ランタイムは `src/runtime/` の独自実装を使用する。
- **`cfg(feature = "...")` を壊さない** — `default = []` のまま、無効 feature でもコンパイル可能に保つ。
- **Linux / カーネル前提**（io_uring、kTLS、seccomp、Landlock、CBPF 等）— README の前提と矛盾させない。
- **ホットパス**でヒープ割り当て・不要なロック・コピー・同期呼び出しを増やさない（詳細は上の **ホットパス絶対規則**）。
- **動的設定**は ArcSwap とリロード経路の不変条件を維持する。
- **`unsafe` は最小限** — 拡大時は不変条件をコメントで明示。

---

## 行動指針

1. 上記 **設計哲学・設計制約** に整合するか確認する。
2. 変更前に、触るコードの `cfg(feature)`、エラーハンドリング、ftlog、serde設定型を読む。
3. 外部契約（設定キー、CLI、メトリクス名、プロトコル範囲）を変えたら **同じ PR で README（必要なら .ja）を更新**する。`specs/` 等を使う場合も矛盾を残さない。
4. 大きなロジックは **専用モジュール**へ。`entry.rs` は配線中心に保つ（`main.rs` は `veil::run()` のみ）。
5. 挙動変更には **単体 / 統合 / E2E** のいずれかを追加または更新し、**`cargo test` で実証**する。

### AI 成果物・ログ・一時ファイル

評価レポート、調査メモ、セッションログ、スクラッチなどは **`docs/artifacts/` にのみ** 置く（他に散乱させない）。無ければ作成してよい。

---

## コーディング規約

- 既存に合わせる（多くは **日本語のモジュール／doc コメント**）。英語への統一リファクタはしない。
- `cargo fmt`、原則 `cargo clippy`。`#[allow(clippy::…)]` を同等の論拠なく増やさない。
- **ホットパスのブロッキング検出（[clippy.toml](clippy.toml) の `disallowed-methods`）**: 同期 `std::fs`・`std::thread::sleep`・ブロッキング `std::net` はデータプレーンで clippy エラー。正当な利用（`runtime::offload` 閉包内・専用スレッド・起動/リロードのコールドパス・テスト/ベンチ）は **理由コメント付きの個別 `#[allow(clippy::disallowed_methods)]`** で明示する（理由なしの追加は禁止）。
- 依存追加は慎重に。[Cargo.toml](Cargo.toml) の記法と feature を尊重。`ftlog` のレベル・頻度は既存に揃える。

---

## 作業フロー

- **1 タスク = 小粒度**（1 PR 1 目的）。
- feature 変更時は `cargo build --no-default-features` および **関連 feature の組み合わせ**で確認。
- プロキシ全体に触れる場合は [tests/e2e_setup.sh](tests/e2e_setup.sh) を参照。

### バックログ

- **機能追加**: [docs/backlog/features/](docs/backlog/features/) に 1 チケット 1 md（機能説明・改修内容・改修案など）。
- **バグ**: [docs/backlog/bugs/](docs/backlog/bugs/) に 1 件 1 md（事象・調査・改修案）。
- **親ドキュメント**: [docs/backlog/backlog.md](docs/backlog/backlog.md) に一覧・優先度・対応状況。チケットの **追加・ステータス変更時は必ず同じ変更で更新**する。

---

## 禁止事項

- 依頼範囲外のドライブバイリファクタ、無関係ファイルの変更。
- README / 仕様を更新せず挙動・設定だけ変えること。
- `default = ["ktls", "http2", "mimalloc"]` を崩して重い依存をデフォルト必須にすること。
- 検証なしの `unsafe` 拡大、安易な `#[ignore]`（やむを得ない場合は理由を文書化）。
- `docs/artifacts/` 以外への AI 専用成果物の散乱。
- バックログの個別 md だけ更新して [docs/backlog/backlog.md](docs/backlog/backlog.md) を更新しないこと。

---

## ビルド・テスト（入り口）

詳細・feature 組み合わせ・E2E・ベンチは **README の Build / Testing 節**を参照。

### 注意事項
- **コンパイル時の依存関係**: `entry.rs`/各モジュールは `http2` や `grpc`（もしくは `grpc-full`）feature が有効でないと、`send_grpc_trailers` の呼び出し箇所等でコンパイルエラーが発生します。そのため、ビルドやテストの際は必ず十分な feature（例：`--features "http2,grpc-full"` またはフルフィーチャー）を指定して実行してください。
- **E2Eテスト**: E2Eテストは専用のバックエンド環境を起動する必要があります。手動で直接 `cargo test` を叩くとバックエンドへの接続ができずタイムアウトするため、必ず `./tests/e2e_setup.sh test` を使用して自動セットアップ・実行・クリーンアップを行ってください。また、ポート競合エラーが発生した場合は、`pkill -f veil` 等で残存プロセスを終了させてから再実行してください。

### 実行コマンド例

```bash
# フル機能でのビルド
cargo build --features "full"

# E2Eテストの実行（自動セットアップ・クリーンアップ付き）
./tests/e2e_setup.sh test

# ユニットテストや統合テストの実行（features指定必須）
cargo test --bins --test integration_tests --features "full"
```

---

## ディレクトリ要約

| パス | 役割 |
|------|------|
| `src/main.rs` | 薄いバイナリエントリ（`veil::run()` を呼ぶだけ） |
| `src/lib.rs` | クレートルート・mod 宣言・公開 API（`cargo fuzz`・統合テスト向け） |
| `src/entry.rs` | サーバ起動配線（`run()`：ワーカースレッド・accept ループなど） |
| `src/runtime/` | 独自 io_uring ランタイム（ring.rs/executor.rs/tcp.rs/timer.rs/buf.rs/io.rs/splice.rs/offload.rs） |
| `tests/`、`benches/` | 統合・E2E・ベンチ（`cargo test` が拾うホワイトボックス） |
| `examples/config.toml` | 設定リファレンス（全キー網羅・`src/config.rs` 同期） |
| `docs/readme/` | 日本語 README（`README.ja.md`） |
| `docs/artifacts/` | AI 成果物・一時ファイル |
| `docs/backlog/` | 機能・バグチケット（親は `backlog.md`） |
| `docker/` | コンテナイメージ（glibc/musl）・共有アセット（`assets/`：ssl/www/seccomp/Landlock） |
| `tools/` | Docker ベースの外形検証ツール。`tools/perf/` は glibc/musl/nginx 比較のパフォーマンス計測ハーネス（`gen_configs.sh` で 2⁴=16 直交表 + full features 機能ショーケース `feat_*` 構成 + **全プロトコル×全機能マトリクス（F-114: `h2_1_proxy_*`/`h3_file_*`/`h3_proxy*`/`grpc_h2_*`/`grpc_h3*`）** を生成 /`run_perf.sh` で反復計測（`CONFIG_GLOB` で scoped 化・gRPC over H3 はクライアント非対応で NA フェイルセーフ・TSV 行順は nginx → veil_glibc → veil_musl を保証、L4 構成は平文 9080 の readiness も確認 = F-118） /`analyze_results.sh` で median±stdev 集計。HTTP/1.1=wrk・HTTP/2=h2load・HTTP/3=QUIC 対応 h2load(`h2load-http3/`)・gRPC/WebSocket=grafana k6(`k6/`) の各クライアントで計測）、`tools/container_security/` はファジング・カオス・h2spec・セキュリティスキャンのオーケストレータ（`run.sh`）。計測結果は `docs/perf/`（サマリ + 計測履歴は `docs/perf/README.md`、生データは `docs/perf/results_raw.tsv` = tools/perf 出力のコピー。詳細な生ログは
`docs/artifacts/perf_reports/` に git 管理外で保持） |

細かいモジュール対応は `src/lib.rs` の `mod` と README の構成を参照。
