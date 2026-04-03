# AGENTS.md — Veil (veil-proxy)

AI エージェントおよびコントリビュータ向けの **最小指針**。機能説明・ビルド例・テスト手順の **正** は [README.md](README.md) / [README.ja.md](README.ja.md)。フィーチャー定義は [Cargo.toml](Cargo.toml) の `[features]`。

---

## プロジェクトの事実

- クレート名 `veil`（ディレクトリ名 `veil-proxy`）。リリースバイナリは `target/release/veil`。
- **バイナリクレート**（`src/lib.rs` なし）。エントリと mod は主に [src/main.rs](src/main.rs)。
- 設定は TOML（`serde`）。ホットリロード・検証の挙動は README を参照。

---

## 設計哲学・こだわりポイント

**Rust の安全性を土台に、Linux カーネル（io_uring、kTLS、seccomp、Landlock、ソケット/CBPF）とユーザー空間を噛み合わせ、HTTP/1.1・2・3 のデータプレーンを tokio なしで高スループット化・ゼロコピー化。開発効率を度外視して最大限の性能を目指し、かつ運用で効く動的設定・観測・拡張（Proxy-Wasm）まで載せる。**

変更やレビューでは、個別機能だけでなく **ここに反しないか** を意識する。

---

## 設計制約（要約）

変更時は **上記の設計哲学** および次の箇条書きに反しないか確認する。

- **データプレーンは tokio に依存しない**（テスト・クライアント用途の tokio は別）。
- **`cfg(feature = "...")` を壊さない** — `default = []` のまま、無効 feature でもコンパイル可能に保つ。
- **Linux / カーネル前提**（io_uring、kTLS、seccomp、Landlock、CBPF 等）— README の前提と矛盾させない。
- **ホットパス**でヒープ割り当て・不要なロック・コピーを増やさない。
- **動的設定**は ArcSwap とリロード経路の不変条件を維持する。
- **`unsafe` は最小限** — 拡大時は不変条件をコメントで明示。

---

## 行動指針

1. 上記 **設計哲学・設計制約** に整合するか確認する。
2. 変更前に、触るコードの `cfg(feature)`、エラーハンドリング、ftlog、serde 設定型を読む。
3. 外部契約（設定キー、CLI、メトリクス名、プロトコル範囲）を変えたら **同じ PR で README（必要なら .ja）を更新**する。`specs/` 等を使う場合も矛盾を残さない。
4. 大きなロジックは **専用モジュール**へ。`main.rs` は配線中心に保つ。
5. 挙動変更には **単体 / 統合 / E2E** のいずれかを追加または更新し、**`cargo test` で実証**する。

### AI 成果物・ログ・一時ファイル

評価レポート、調査メモ、セッションログ、スクラッチなどは **`docs/artifacts/` にのみ** 置く（他に散乱させない）。無ければ作成してよい。

---

## コーディング規約

- 既存に合わせる（多くは **日本語のモジュール／doc コメント**）。英語への統一リファクタはしない。
- `cargo fmt`、原則 `cargo clippy`。`#[allow(clippy::…)]` を同等の論拠なく増やさない。
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
- `default = []` を崩して重い依存をデフォルト必須にすること。
- 検証なしの `unsafe` 拡大、安易な `#[ignore]`（やむを得ない場合は理由を文書化）。
- `docs/artifacts/` 以外への AI 専用成果物の散乱。
- バックログの個別 md だけ更新して [docs/backlog/backlog.md](docs/backlog/backlog.md) を更新しないこと。

---

## ビルド・テスト（入り口）

詳細・feature 組み合わせ・E2E・ベンチは **README の Build / Testing 節**を参照。

```bash
cargo build --features "ktls,http2,http3,grpc-full,wasm"

./tests/e2e_setup.sh start
cargo test --features "ktls,http2,http3,grpc-full,wasm"
./tests/e2e_setup.sh stop
```

---

## ディレクトリ要約

| パス | 役割 |
|------|------|
| `src/main.rs` | エントリ・mod 宣言・コア HTTP/1 など（下位モジュールは同 `src/` 配下） |
| `tests/`、`benches/` | 統合・E2E・ベンチ |
| `docs/artifacts/` | AI 成果物・一時ファイル |
| `docs/backlog/` | 機能・バグチケット（親は `backlog.md`） |
| `docs/specs/` | 企画書・仕様書 |
| `docs/instructions/` | AI指示書 |

細かいモジュール対応は `src/main.rs` の `mod` と README の構成を参照。

---

## 更新履歴

詳細な変更履歴は Git を参照。

- 設計哲学・こだわりポイントを本文に集約し、`docs/philosophy.md` を廃止。
