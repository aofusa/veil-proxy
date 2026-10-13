# veil-tokio（F-198）

veil の tokio 互換レイヤーの置き場。実装は veil 本体の `veil::tokio`（feature `tokio-compat`、API ライブラリ専用）に
あり、ここには依存ツリーの tokio を置き換えるための `tokio-shim` だけを置く（veil の実装がランタイムの内部
`runtime::offload::await_completion` 等を使うため、別クレートに分けると循環するので本体に入れている）。

```toml
# 利用側の Cargo.toml
[dependencies]
veil = { git = "...", default-features = false, features = ["full", "api"] }
hyper = { version = "1", features = ["server", "http1"] }   # tokio を使うクレート

[patch.crates-io]
tokio = { git = "...", package = "tokio" }   # crates/veil-tokio/tokio-shim（パッケージ名 tokio、版 1.99.0）
```

`tokio-shim` は `veil::tokio::*` を再エクスポートし、tokio の機能フラグ（`full`・`rt-multi-thread`・`net` 等）は
すべて受け付けるだけ。互換性の範囲と未対応の API は `docs/guide/library.md` の互換表を参照。
