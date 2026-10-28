# B-205: 導入例の依存の版が存在しない "0.9" を指し、bump-version.sh が major.minor の例を更新しない

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の Python・C バインディング・リリースのレビュー（docs/artifacts/v0100/review-bindings-release.md） #4（2026-10-26）。

## 事象

- README・ガイド・rustdoc の `veil-proxy-kit = { package = "veil-proxy-kit", version = "0.9" }` 等の導入例は、
  `tools/release/bump-version.sh` が `x.y.z` しか書き換えないため古い版のまま残り、0.10 で新しく出るクレート
  （veil-proxy-kit 等）は存在しない版を指していた。

## 修正

- `bump-version.sh` に、追跡しているファイル（`.md`・`.toml`・`.rs`・`.html`。履歴・リリースノート・バックログ・perf・
  artifacts・third_party は除く）の `package = "veil-proxy…", version = "X.Y"` を新しい major.minor にする段を足した。
- それを 0.10.0 で走らせて現在の例（12 ファイル）を `"0.10"` にした。

## 試験

- 作業用の複製で `bump-version.sh 0.11.0` を走らせ、例が `"0.11"` になり、履歴等が変わらないこと。`bash -n`。

## 実施内容

- `tools/release/bump-version.sh`、README・`crates/*/README.md`・`docs/guide/library*.md`（英日）・`docs/readme/README.ja.md`・
  `examples/api/rust-kit/Cargo.toml`・`src/lib.rs`。
