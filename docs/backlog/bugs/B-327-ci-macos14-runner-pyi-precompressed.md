# B-327: CI の macOS arm64 のランナー（macos-14）が廃止、Python の型スタブに `precompressed` が無い、試験の値が秘密の検出に掛かる

**状態: 修正済み（v0.11.0）**

## 事象

v0.11.0 の PR（#10）の CI で 3 件が失敗した。

- `bindings macos-arm64`: `The configuration 'macos-14-arm64-us-default' is not supported`（GitHub が macos-14 のイメージを廃止）。
  `release.yml`・`dist.yml` の macOS arm64 も同じランナー。
- `python docs + stubs (F-239)`: stubtest が `veil._veil.register_static` の `precompressed`（F-294 で足した引数）がスタブに無いと報告。
- GitGuardian: `crates/veil-capi-helpers/src/misc.rs` の試験のフォームの値（`username=...&password=...`）を資格情報として検出（偽陽性）。

## 修正

- macOS arm64 のランナーを `macos-15` にした（`bindings.yml`・`dist.yml`・`release.yml`）。
- `crates/veil-python/python/veil/_veil.pyi` の `register_static` に `precompressed: list[str] = ...` を足した。
- 試験のフォームの名前を `name`・`code` にした（試験の中身は `+` と `%XX` の復号で同じ）。検出済みの履歴の 1 件は GitGuardian の
  ダッシュボードで偽陽性として閉じる（人の作業）。
