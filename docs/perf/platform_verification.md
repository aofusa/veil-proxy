# v0.7.0 全プラットフォーム検証

リリース前に、対象のすべての OS・アーキテクチャで単体・統合・E2E を回した結果（2026-10-06〜08）。
最後のコード変更は HTTP/2 の不正プリフェース処理（B-102、`0ff5442`。エラー経路のみ）と、Linux 専用の
seccomp 許可リストの番号修正（B-103、`3c41170`）。B-102 は全 OS、B-103 は Linux（x86_64 / aarch64）で
回し直している。`c881dd9` の行は B-102 を含まないが、B-102 の変更は全プラットフォーム共通のコードで、
同じコードを Linux・macOS・Windows・FreeBSD/OpenBSD/NetBSD x86_64 で確認済み。
Linux 以外は実機（macOS・Windows）または実カーネルの VM（`tools/qemu/`）上で、ネイティブビルドしたものを
テストしている。

| OS | アーキ | 環境 | feature | 単体 | 統合 | E2E | コミット |
|---|---|---|---|---|---|---|---|
| Linux | x86_64 | ホスト（io_uring） | `full` | 995 | 54 | 555 | `0a96b10` |
| Linux | x86_64 | ホスト（epoll reactor） | `full,epoll` | — | — | 555 | `0a96b10` |
| Linux | aarch64 | QEMU + HVF（Ubuntu、io_uring） | `full` | 993 | 54 | 555 | `3c41170` |
| macOS | aarch64 | 実機（kqueue reactor） | `full` | 943 | 54 | 555 | `0ff5442` |
| Windows | x86_64 | 実機（WSAPoll reactor） | `full` | 902 | 54 | スモーク※ | `0ff5442` |
| FreeBSD | aarch64 | QEMU + HVF（14.3） | `full-freebsd` | 952 | 54 | 555 | `c881dd9` |
| FreeBSD | x86_64 | QEMU + KVM（14.3） | `full-freebsd` | 953 | 54 | 555 | `0ff5442` |
| OpenBSD | aarch64 | QEMU + HVF（7.9） | `full-openbsd` | 940 | 54 | 554（無視 1） | `c881dd9` |
| OpenBSD | x86_64 | QEMU + KVM（7.9） | `full-openbsd` | 941 | 54 | 554（無視 1） | `0ff5442` |
| NetBSD | aarch64 | QEMU + HVF（10.1） | `full-netbsd` | 900 | 53 | 429 | `c881dd9` |
| NetBSD | x86_64 | QEMU + KVM（10.1） | `full-netbsd` | 941※2 | 54 | 555 | `d837bc5`（E2E は `0ff5442`） |

すべて失敗 0。件数の違いは feature とプラットフォームの `cfg` による（Windows は Unix 専用の
テストが、NetBSD は `full-netbsd` に含まれない機能のテストが対象外）。

※2 NetBSD x86_64 の単体は、ビルド直後の 1 回目に `wasm::host::grpc_executor::tests::measure_pooled_vs_fresh_connection`
（100 本の新規接続で gRPC を呼ぶ計測テスト。応答待ち 5 秒）がタイムアウトすることがあった（2 回）。同じバイナリで
単独 5 回・全体 3 回を回し直してすべて通過したため、負荷の高い起動直後の時間依存と判断した。

※ Windows には bash の E2E ハーネスが無いため、前段（静的配信 + プロキシ）と後段の veil を 2 つ起動して
疎通を確認した（静的 200・プロキシ 200・後段停止時 502）。

## ビルド

Linux x86_64 で `default` / `--no-default-features` / `full` / `full-container` と、各 feature 単独
（`--no-default-features --features <f>`）をビルドし、警告ゼロ。clippy は `full` と `full,epoll` の
`--all-targets -D warnings` で指摘ゼロ。macOS でも `full` の clippy `-D warnings` が通る。

`aio` 単独は Linux では build.rs が意図的に止める（FreeBSD 専用。POSIX AIO の経路）。

| ビルド | 結果 |
|---|---|
| `default` / `--no-default-features` / `full` / `full-container` | 警告 0 |
| `--no-default-features --features <f>`（ktls・http2・http3・epoll・wasm・grpc・grpc-web・grpc-full・mimalloc・jemalloc・system-allocator・alloc-stats・compression・cache・metrics・websocket・rate-limit・buffering・opentelemetry・admin・access-log・l4-proxy） | すべて警告 0 |
| `cargo clippy --all-targets -D warnings`（`full` / `full,epoll`） | 指摘 0 |
| `cargo fmt --check` | 差分なし |

## 検証の過程で見つけて直した不具合

Linux の既定ビルドでは 1 行もコンパイルされない経路（reactor、BSD の暗号・ソケット・アロケータ、
Windows の Winsock）で、それまでのテストをすり抜けていたもの。詳細は各チケット。

| チケット | 見つかった環境 | 内容 |
|---|---|---|
| [B-86](../backlog/bugs/B-86-http3-tls-backend-plaintext-overflow.md) | macOS | HTTP/3 の TLS 上流で応答本文が途中で切れ、200 のまま終わる |
| [B-87](../backlog/bugs/B-87-macos-nofile-limit-not-raised.md) | macOS | fd 上限が 256 のまま |
| [B-88](../backlog/bugs/B-88-windows-unit-tests-and-platform-gaps.md) | Windows | 単体テストがコンパイル不能。ディレクトリ open と L4 半クローズの本番不具合 2 件 |
| [B-89](../backlog/bugs/B-89-freebsd-jemalloc-hooks-and-static-containment.md) | FreeBSD | jemalloc が libthr のフックを上書きして SIGSEGV。静的配信の封じ込め |
| [B-90](../backlog/bugs/B-90-tls-handshake-drops-coalesced-records.md) | FreeBSD | TLS ハンドシェイクで 4KB 超の暗号文を捨てる（長年のフレーク B-68 の正体） |
| [B-91](../backlog/bugs/B-91-netbsd-connect-timeout-refused-ok.md) | NetBSD | 落ちた上流を healthy と判定 |
| [B-93](../backlog/bugs/B-93-pooled-upstream-closed-while-idle.md) | Linux（E2E のフレーク） | 上流が閉じたプール接続を再利用して 502 |
| [B-94](../backlog/bugs/B-94-http3-retransmitted-initial-new-connection.md) | FreeBSD（perf） | HTTP/3 の Initial 再送で別接続を作る |
| [B-95](../backlog/bugs/B-95-bsd-udp-socket-buffer-enobufs.md) | FreeBSD（perf） | BSD で UDP バッファ拡大が ENOBUFS で失敗し 42KB のまま |
| [B-98](../backlog/bugs/B-98-pulley-unoptimized-in-debug-builds.md) | FreeBSD（E2E） | デバッグビルドの Pulley が 100 倍遅く HTTP/3 + WASM の E2E がタイムアウト |
| [B-100](../backlog/bugs/B-100-http3-malformed-request-accepted.md) | Linux（container_security の h3spec） | HTTP/3 で疑似ヘッダ違反のリクエストに 200 を返す |
| [B-102](../backlog/bugs/B-102-h2c-invalid-preface-rst.md) | Linux（container_security の h2spec） | h2c で不正なプリフェースに GOAWAY を返さず RST |
| [B-103](../backlog/bugs/B-103-seccomp-wrong-syscall-numbers.md) | Linux（container_security の E2E サニタイザ） | seccomp 許可リストの syscall 番号ずれ（x86_64 の prctl など） |

未対応で残したもの: [B-97](../backlog/bugs/B-97-http3-wasm-blocks-main-loop.md)（HTTP/3 の WASM 実行が
メインループを止める。Pulley ターゲットで顕在化。設計変更が必要）、
[B-101](../backlog/bugs/B-101-tls-connection-memory-footprint.md)（同時 TLS 接続あたり約 200〜250KB。
64MB 以下のコンテナで OOM）。
