# 全プラットフォーム検証（v0.11.0）

v0.11.0 の候補（`feat/v0110-tickets`）について、対象のすべての OS・アーキテクチャで単体・統合・E2E の試験を回した結果
（2026-11-05〜06）。

- Linux x86_64 以外は、実機（macOS・Windows）か実カーネルの VM（`tools/qemu/`）の上で、ネイティブにビルドしたものを試験している。
- 最終候補は `325e92c3`（v0.11.0 前のセキュリティスキャンの修正 B-313〜B-325 を含む）。Linux x86_64・macOS・Windows・FreeBSD aarch64 は
  このコミットで回し直した。他の BSD・Linux aarch64 の行はセキュリティ修正の前のコミット（`ce33e27b`・`650d6695`）の結果で、
  その後の変更は上の 4 つと各 OS の clippy で確かめた（BSD の固有の経路は FreeBSD の capsicum の security-e2e と、`cfg(not(target_os = "linux"))` の
  経路を共有する macOS・Windows の E2E）。

| OS | アーキ | 環境 | feature | コミット | 単体 | 統合 | E2E |
|---|---|---|---|---|---|---|---|
| Linux | x86_64 | ホスト（io_uring） | `full` | `325e92c3` | 1277 | 63 | 592 |
| Linux | x86_64 | ホスト（epoll reactor） | `full,epoll` | `325e92c3` | — | — | 592 |
| Linux | aarch64 | QEMU + HVF（Ubuntu、io_uring・epoll） | `full` | `ce33e27b`（E2E はその直前） | 1257 | 63 | 590・590 |
| macOS | aarch64 | 実機（kqueue reactor） | `full-macos` | `325e92c3` | 1174 | — | 591 |
| Windows | x86_64 | 実機（WSAPoll reactor） | `full-windows` | `325e92c3` | 1121 | — | 577 |
| FreeBSD | aarch64 | QEMU + HVF（14.3） | `full-freebsd` | `325e92c3` | 1197 | 63 | 591 |
| FreeBSD | x86_64 | QEMU + KVM（14.3） | `full-freebsd` | `ce33e27b` | 1180 | 63 | 589 |
| OpenBSD | aarch64 | QEMU + HVF（7.9） | `full-openbsd` | `ce33e27b` | 1154 | 63 | 588（無視 1） |
| OpenBSD | x86_64 | QEMU + KVM（7.9） | `full-openbsd` | `ce33e27b` | 1154 | 63 | 587（無視 1）※1 |
| NetBSD | aarch64 | QEMU + HVF（10.1） | `full-netbsd` | `ce33e27b`・`650d6695`（E2E） | 1154 | 63 | 589 |
| NetBSD | x86_64 | QEMU + KVM（10.1） | `full-netbsd` | `ce33e27b` | 1155※2 | 63 | 589 |

すべて失敗 0（下の注のとおり、試験の側・環境を直して回し直したものを含む）。

- 件数の違いは、feature とプラットフォームの `cfg` による（Windows は Unix 専用の試験、BSD は `full-*` に含まれない機能の試験が対象外）。
- BSD の 3 OS は両アーキで OS 固有のサンドボックス（FreeBSD の capsicum・OpenBSD の pledge/unveil・NetBSD の chroot + 特権降格）の下での
  E2E（`bsd-vm.sh <os> <arch> security-e2e`）も通った。FreeBSD aarch64 は最終候補でも通った。
- Windows の ACME の鍵ファイルの DACL（B-325）は `acme::tests::secret_files_are_owner_only` を Windows 実機で通した。
- ライブラリ（`tests/api/library_e2e.sh all`、Linux x86_64、`325e92c3`）: 952 項目すべて ok。
- Windows の E2E は v0.11.0 から bash のハーネス（`tests/e2e_setup.sh`、Git Bash）でそのまま走る（B-311。UDS の上流の試験は unix だけ）。

※1 OpenBSD x86_64 の `test_tls_cipher_suites_rejects_excluded_suite` は 1 回目に 15.05 秒で試験の時間の上限（15 秒、TCG の負荷）を
越えた。その試験だけを 3 回回し直して 3 回とも通った。

※2 NetBSD x86_64 の単体の 1 回目は、Proxy-Wasm の締め切りの試験 1 件が VM の負荷で落ちた。回し直しで全件通った。

NetBSD aarch64 の E2E の 1 回目は B-306 の試験（大きな送信）が落ちた。1 回に送る量を 16KB ほどにして（`650d6695`）、その試験を 3 回と
E2E の全体を回し直して通った。

### ビルド・静的解析（Linux x86_64）

| 対象 | 結果 |
|---|---|
| `cargo clippy -D warnings`: `default`・`--no-default-features`・`full`・`full,epoll`・`full-container`・`full,api`・`full,handler,mesh`・各 feature の単独（`--no-default-features --features <f>`）・ワークスペースの各クレート・`veil-proxy-kit` の各 feature（`docs/artifacts/v0110/warnings/sweep.sh`） | 警告・指摘 0（`--features aio` だけは build.rs が FreeBSD 以外で意図して止める。`mesh-linkerd` の単独は B-326 を直して通った） |
| macOS（`full-macos`）・Windows（`full-windows`）の clippy `-D warnings` | 指摘 0 |
| `cargo fmt --check` | 差分なし |
| `#[allow(dead_code)]` | 追加なし |
| `tools/lint/check-messages.py`・`tools/release/check-docs.sh` | 0 件 |

### セキュリティ

- `tools/container_security/` のフル（拡張・サニタイザ込み）を通した（2026-11-06、セキュリティスキャンの修正の前の候補）。
- Claude Security のスキャン（`src/`）の 57 件（検証済み）は B-313〜B-325 で全件直した。
