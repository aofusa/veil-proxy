# B-287: Homebrew 7 が信頼していない tap の formula を読まず、公開の試験（brew audit）が落ちる

**状態: 修正済み（v0.10.1）**

## 出典

v0.10.1-rc.4 の公開の試験（`release.yml` の `dist / homebrew ubuntu-24.04-arm`、2026-11-04）。

## 事象

`tools/release/homebrew-test.sh` の `brew audit --strict aofusa/localtest/veil` が
`Refusing to load formula aofusa/localtest/veil from untrusted tap aofusa/localtest.` で失敗した（rc.3 までは通っていた）。
install・test・reinstall・uninstall は通った。

## 原因

Homebrew 7（macOS で 7.0.10 を確認）から、公式でない tap の formula は `brew trust` で信頼したものしか読まない（既定。
`HOMEBREW_NO_REQUIRE_TAP_TRUST` は非推奨）。例外はコマンド行に完全な名前（`aofusa/tap/veil`）を書いた場合だけで、短い名前
（`brew services start veil`）・`brew upgrade`（信頼していない tap は飛ばす）・tap 全体を読む `brew audit` は読まない。
利用者向けの手順（README・`running.md`・tap の README・リリースノート）も `brew trust` を書いていなかった。

## 修正（v0.10.1）

- `tools/release/homebrew-test.sh`・tap の CI（`packaging/homebrew/tap-test.yml`）: `brew trust` があれば tap を信頼してから試す。
- 文書: `brew install aofusa/tap/veil` の後に `brew trust aofusa/tap`（Homebrew 7 以降）を足す（`README.md`・
  `docs/readme/README.ja.md`・`docs/guide/running.md`・`ja/running.md`・`packaging/homebrew/tap-README.md`・`docs/releases/v0.10.1.md`）。

## 関連

- [F-250](../features/F-250-homebrew.md)
