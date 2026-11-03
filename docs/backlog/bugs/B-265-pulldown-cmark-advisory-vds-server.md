# B-265: VDS の参照サーバの Cargo.lock の pulldown-cmark 0.13.5 の勧告で release の検査が落ちる

**状態: 修正済み（v0.10.0）**

## 出典

`v0.10.0-rc.3`（`0ef8f3e4`）の `release.yml` の `dependency advisories`（run 41657341944）と、同じコミットの
`container-security-nightly`（run 41554607050、glibc・musl の `cargo_audit`）が RUSTSEC-2026-0433（high、2026-10-28 公開）で失敗した。
rc.2 の時点ではまだ勧告が無かった。

## 事象と修正

| 事象 | 原因 | 修正 |
|---|---|---|
| `tools/vds/server/Cargo.lock` の `pulldown-cmark@0.13.5` に RUSTSEC-2026-0433（`ENABLE_WIKILINKS` の入れ子の画像のウィキリンクで解析が 2 乗の時間） | `tonic-prost-build` → `prost-build` のビルド依存（proto のコメントの Markdown 変換）。veil 本体・配布物には入らない | `cargo update -p pulldown-cmark --precise 0.13.6`（`Cargo.lock` だけ。`cargo check --locked` が通る） |

## 影響

- P1（公開を止める）: cargo-audit はリポジトリ内の全 `Cargo.lock` を見る（B-158）ので、公開の検査が通らない。
  到達性は無い（ビルド時に自前の proto のコメントを変換するだけ）。

## 関連

- [B-263](B-263-nightly-advisories-and-musl-test-build-v0100.md)、[F-276](../features/F-276-v0100-verification-and-release.md)。
