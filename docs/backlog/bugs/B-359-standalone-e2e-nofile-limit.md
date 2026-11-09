# B-359: 自己完結型 E2E が fd の上限を上げておらず、OpenBSD の既定で `EMFILE` になる

**状態: 修正済み（feat/v0120-perf、2026-11-10）**

v0.12.0 の最後の検証で OpenBSD x86_64 の自己完結型 E2E を回して見つけた。試験の側の問題で、veil には関係しない（veil は起動時に自分の上限を上げる）。

## 事象

OpenBSD 7.9 x86_64 の `cargo test --test standalone_e2e` で、`b172_route_cache_keeps_earlier_header_and_query_routes` が試験のクライアントの
tokio のランタイムの作成（`Builder::build().unwrap()`）で `EMFILE`（`Too many open files`）になって落ちた。どの試験が落ちるかは並列の実行の順で変わる。

## 原因

OpenBSD の ssh の root のセッションは fd のソフトの上限が 128（ハードは 1024）。`tests/e2e_setup.sh` は `ulimit -n 65536` で上げているが、
自己完結型 E2E は `cargo test` を直接呼ぶので上がらず、1 プロセスの中で並列に動く試験の接続・ランタイム・一時ファイルで足りなくなる。

## 対応

`tests/standalone/mod.rs` の `raise_nofile_limit()` が、この試験のプロセスのソフトの上限を、ハードの上限（65536 まで）へ最初の 1 回だけ上げる。
`free_port()` と `VeilProcess::spawn_with()` の入口で呼ぶ。上げられないときはそのまま続ける（macOS はハードが無制限でも `OPEN_MAX` を超える値を拒否する）。
`veil` の子プロセスもこの上限を引き継ぐ。

## テスト

修正の後の自己完結型 E2E はすべて通った。OpenBSD x86_64（49 件を 2 回）・OpenBSD aarch64（49 件）・NetBSD x86_64・aarch64（49 件）・FreeBSD x86_64（50 件）・macOS（50 件）・Linux x86_64 の `full`・`full,epoll`（52 件）。結果の一覧は [platform_verification.md](../../perf/platform_verification.md)。
