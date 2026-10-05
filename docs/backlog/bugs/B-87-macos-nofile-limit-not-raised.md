# B-87: macOS で起動時の RLIMIT_NOFILE 引き上げが常に失敗し、fd 上限が 256 のまま残る

## 事象

macOS 実機で `cargo test --lib` を回すと
`pool::tests::http_connection_pool::test_put_respects_max_idle_256` が
`socketpair failed` で失敗した（256 本超の fd を同時に保持するテスト）。

調べると本番コードにも同じ問題があった。`system::raise_nofile_limit`（B-44 第4段）は
soft limit を hard limit まで引き上げるが、**macOS の hard limit は既定で
`RLIM_INFINITY`** で、`setrlimit(2)` は soft に `kern.maxfilesperproc` を超える値
（`RLIM_INFINITY` を含む）を渡すと **EINVAL** を返す（setrlimit(2) の
COMPATIBILITY 節）。warn を出して続行する fail-open 実装のため、**macOS では
fd 上限が既定の 256 のまま**動き、数百接続で EMFILE になる。

## 修正

macOS では soft に設定する値を `sysctlbyname("kern.maxfilesperproc")` で丸める
（`src/system.rs` の `macos_maxfilesperproc`）。他 OS は無変更。
テスト側も本番の起動処理と同じく `raise_nofile_limit()` を呼んでから 256 本超の
fd を作るようにした。

## 検証

macOS 実機で単体 924 件・統合 54 件すべて通過。
