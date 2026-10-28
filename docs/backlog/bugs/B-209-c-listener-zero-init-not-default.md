# B-209: C の veil_listener_t は 0 初期化で既定にならず、拡張の余地が無い

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の Python・C バインディング・リリースのレビュー（docs/artifacts/v0100/review-bindings-release.md） #11（2026-10-26）。

## 事象

- `veil_listener_t` の `http2`・`allow_http`・`allow_h2c`・`secure_redirect` は「負 = 既定、0 = false」だったため、
  C の指定初期化子（`{ .listen = ... }`）で省いたフィールドが false になり（平文で HTTP/1.1 も h2c も受けない等）、
  既定を得るには全フィールドに -1 を書く必要があった。大きさ・版のフィールドも無く、フィールドを足すと ABI が壊れた。

## 修正

- 3 値を「0 = 既定（`VEIL_DEFAULT`）、正 = true（`VEIL_ON`）、負 = false（`VEIL_OFF`）」にし、0 で埋めたものが既定になる。
- 先頭に `size_t size` を足した（`VEIL_LISTENER_INIT` が `sizeof` を入れる。0 はこの版の大きさ）。ライブラリは `size` までを
  0 で埋めた構造体へ写して読むので、後の版で末尾にフィールドを足しても古いヘッダでビルドしたプログラムは動く。
  この版より小さい `size` は `VEIL_ERR_INVALID_ARG`。0.10 で初めて出る API なので今変える。

## 試験

- `crates/veil-capi/src/lib.rs` の `zeroed_listener_means_defaults`（0 埋めが既定・3 値・小さい size の拒否・大きい size の
  読み飛ばし・listen 無しの拒否）と、既存の `add_listener_builds_the_listener_array` を新しい値に。

## 実施内容

- `crates/veil-capi/include/veil.h`・`src/lib.rs`・`README.md`、`docs/guide/library.md`（英日）。
