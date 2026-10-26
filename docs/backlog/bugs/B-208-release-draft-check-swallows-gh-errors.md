# B-208: release.yml の公開済みの判定が gh のどんな失敗も「draft 無し」とみなす

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の Python・C バインディング・リリースのレビュー（docs/artifacts/v0100/review-bindings-release.md） #10（2026-10-26）。

## 事象

- B-168 の判定（公開済みのリリースを上書きしない）は `gh release view` の失敗をすべて「リリース無し」とみなしていたため、
  認証・API の一時的な失敗のときに公開済みのリリースを draft として作り直そうとし得た。

## 修正

- `tools/release/release-state.sh` を足し、`published`・`draft`・`none` を出す。`release not found` だけを `none` とし、
  他の失敗は終了コード 1 で止める。`release.yml` と `github-draft.sh` はこれを使う。`--self-test`（偽の gh）を CI の
  messages ジョブで走らせる。

## 試験

- `tools/release/release-state.sh --self-test`（公開済み・draft・無し・その他の失敗）。

## 実施内容

- `tools/release/release-state.sh`（新規）、`.github/workflows/release.yml`・`ci.yml`、`tools/release/github-draft.sh`。
