# 全プラットフォーム検証（v0.10.0）

v0.10.0 の候補（`feat/v0100`）について、対象のすべての OS・アーキテクチャで単体・統合・standalone・E2E・ライブラリの試験を回した結果
（2026-10-26〜31）。

- Linux x86_64 以外は、実機（macOS・Windows）か実カーネルの VM（`tools/qemu/`）の上で、ネイティブにビルドしたものを試験している。
- 最終候補は `83b94152`。Linux x86_64 と BSD x86_64 はこのコミットで全部を回し直した。他の行は途中のコミットの結果で、その後に入った
  変更（セキュリティレビューの修正・feature の組み合わせの修正・試験のハーネス）は、Linux の両バックエンド・kTLS・seccomp strict の
  E2E と、各 OS の clippy・単体（macOS・Windows）、CI（PR #1）で確かめた。

| OS | アーキ | 環境 | feature | コミット | 単体 | 統合 | standalone | E2E | ライブラリ |
|---|---|---|---|---|---|---|---|---|---|
| Linux | x86_64 | ホスト（io_uring） | `full` | `83b94152` | 1216 | 63 | 36 | 580 | すべて ok |
| Linux | x86_64 | ホスト（epoll reactor） | `full,epoll` | `83b94152` | — | — | — | 580 | — |
| Linux | x86_64 | ホスト（kTLS・seccomp strict） | `full`（+ `epoll`） | `83b94152` | — | — | — | 580・580・580 | — |
| Linux | aarch64 | QEMU + HVF（Ubuntu、io_uring・epoll） | `full` | `918265dc` | 1167 | 63 | — | 570・570 | — |
| macOS | aarch64 | 実機（kqueue reactor） | `full-macos` | `257f4698` | 1113 | 63 | 34 | 570※1 | すべて ok※1 |
| Windows | x86_64 | 実機（WSAPoll reactor） | `full-windows` | `05dd86b6` 前後 | 1187（`api` 込み） | 57※1 | 28※1 | スモーク※2 | kit・C ok※1 |
| FreeBSD | aarch64 | QEMU + HVF（14.3） | `full-freebsd` | 10-26〜27・`83b94152`（python） | 1087 | 63 | — | 570 | すべて ok※3 |
| FreeBSD | x86_64 | QEMU + KVM（14.3） | `full-freebsd` | `83b94152` | 1136 | 63 | — | 579 | — |
| OpenBSD | aarch64 | QEMU + HVF（7.9） | `full-openbsd` | 10-26〜27・`83b94152`（websocket） | 1064 | 63 | — | 569（無視 1） | すべて ok※3 |
| OpenBSD | x86_64 | QEMU + KVM（7.9） | `full-openbsd` | `83b94152` | 1112 | 63 | — | 578（無視 1） | — |
| NetBSD | aarch64 | QEMU + HVF（10.1） | `full-netbsd` | 10-26〜27・`83b94152`（python） | 1064 | 63 | — | 570 | すべて ok※3 |
| NetBSD | x86_64 | QEMU + KVM（10.1） | `full-netbsd` | `83b94152` | 1112 | 63 | — | 579 | — |

すべて失敗 0（下の注のとおり、試験の側・環境を直して回し直したものを含む）。

- 件数の違いは、feature とプラットフォームの `cfg` による（Windows は Unix 専用の試験、BSD は `full-*` に含まれない機能の試験が対象外）。
  E2E の 570 → 580 は途中で足した試験（B-244〜B-262・F-282）の分。
- ライブラリは `tests/api/library_e2e.sh`（`all` = nonregression・Rust・GraphQL・gRPC・アップグレード・WebSocket・kit・C・Python・本番向けの
  設定）。Linux はこれに `cargo test -p veil-capi`（8）・`-p veil-proxy-kit --features full`（28 + 14）を足した。
- BSD の 3 OS は両アーキで OS 固有のサンドボックス（FreeBSD の capsicum・OpenBSD の pledge/unveil・NetBSD の chroot + 特権降格）の下での
  E2E（`bsd-vm.sh <os> <arch> security-e2e`）も通った。FreeBSD x86_64 は capability mode の E2E（`fbsd-capmode-e2e.sh`）も通った。
- WSL（Windows 上の Linux、root）で、root でしか走らない試験（サンドボックスのマウント・capability の落とし方・veil-alloc）を回した。

※1 macOS の E2E・ライブラリ（10-26・10-27）と、Windows の統合・standalone・ライブラリ（10-26）は途中のコミットの結果。Windows の
ライブラリの 1 回目は kit の HTTPS クライアントの試験が落ちたが、回し直しで kit・C とも通った。PR の CI（`bindings` の Windows・macOS・
Linux）は最終候補の直前まで毎回 `c`・`python` を通している。

※2 Windows は bash の E2E のハーネスが UDS のバックエンドなど Windows で使えない機能を含むので、`veil.exe` を前段（静的配信 + `/proxy/`
の中継）と後段の 2 つ起動して確かめた: 静的配信 200・h2c 200（HTTP/2）・中継 200（`localhost` 経由も）・ファイルの置き換えと新しい
ディレクトリの変更通知（HTTP/1.1・h2c）・後段を止めた後 502・panic なし。

※3 BSD aarch64 のライブラリは 1 回目にいくつか落ち、原因を直して回し直した。
- OpenBSD: WebSocket の 2,000 本・asyncio の 2,000 並行が fd の上限（root のハード上限 1024）と listen の待ち行列（既定 128 で溢れた SYN に RST）
  で落ちた。`library_e2e.sh` が root のときに上限と `kern.somaxconn` を上げるようにした（B-215）。GraphQL・tower・maturin のビルド・`head -c` の失敗は
  回し直し（rust・kit・python・production）で通った。
- NetBSD・FreeBSD: 100 MB のアップロードの後の常駐メモリの増え方が上限を超えた。libc の jemalloc がアイドル時に返さないためで、
  `MALLOC_CONF` の既定を試験で設定し、利用者向けの文書でも勧めるようにした（`docs/guide/library-production.md`）。最終候補で回し直して
  FreeBSD・NetBSD とも +4 MiB（FreeBSD は直後に +81 MiB で、アイドル時に返る）。OpenBSD の WebSocket の 2,000 本も最終候補で通った。
- NetBSD の asyncio の 2,000 並行の 1 本の RST は、上の listen の待ち行列の修正の後の回し直しで通った。

### ビルド・静的解析（Linux x86_64）

| 対象 | 結果 |
|---|---|
| `cargo clippy --all-targets -D warnings`: `default`・`--no-default-features`・`full`・`full,epoll`・`full-container`・`full,api`・メッシュ・各 feature の単独（`--no-default-features --features <f>`）・ワークスペースの各クレート・`veil-proxy-kit` の各 feature（`docs/artifacts/v0100/warnings/sweep.sh`） | 全組み合わせで警告・指摘 0 |
| macOS（`full-macos`）・Windows（`full-windows`、`api` 込み）の clippy `--all-targets -D warnings` | 指摘 0 |
| `cargo fmt --check` | 差分なし |
| `#[allow(dead_code)]` | 追加なし |
| `tools/lint/check-messages.py`・`tools/release/check-docs.sh` | 0 件 |
| ファジング（新しい 4 本: `h3_field_section`・`http2_connection_state`・`http_response_parse`・`path_norm`。各 60 秒） | クラッシュなし |
