# 全プラットフォーム検証（v0.12.0）

v0.12.0 の候補（`feat/v0120-perf`）について、対象のすべての OS・アーキテクチャで単体・統合・E2E の試験を回した結果
（2026-11-09〜10）。

- Linux x86_64 以外は、実機（macOS・Windows）か実カーネルの VM（`tools/qemu/`）の上で、ネイティブにビルドしたものを試験している。
- 最終候補は `df6b5aae`。`src/` は `bb9b4fec` と同じ（`df6b5aae` は試験用クレートの `cargo-audit` の除外と文書だけ）。
  `df6b5aae` より後のコミットは計測のスクリプト・試験（[B-358](../backlog/bugs/B-358-vds-server-aws-lc-openbsd-xonly.md)・[B-359](../backlog/bugs/B-359-standalone-e2e-nofile-limit.md)）・文書だけで、`src/` は変わらない。
  `60fc2a63` → `bb9b4fec` の差は io_uring の待機の経路（[B-354](../backlog/bugs/B-354-offload-drain-loses-queued-poll.md) の 1 周回に 1 回の読み捨て。
  `cfg(veil_rt_uring)`）だけなので、io_uring を使わない OS・バックエンドの行は `60fc2a63` の結果のままにした。

| OS | アーキ | 環境 | feature | コミット | 単体 | 統合 | E2E | 自己完結型 E2E |
|---|---|---|---|---|---|---|---|---|
| Linux | x86_64 | ホスト（io_uring） | `full` | `bb9b4fec` | 1310 | 63 | 592 | 52 |
| Linux | x86_64 | ホスト（epoll reactor） | `full,epoll` | `bb9b4fec` | — | — | 592 | 52 |
| Linux | aarch64 | QEMU + HVF（Ubuntu、io_uring） | `full` | `df6b5aae` | 1306 | 63 | 592 | — |
| Linux | aarch64 | QEMU + HVF（Ubuntu、epoll reactor） | `full,epoll` | `60fc2a63` | — | — | 592 | — |
| macOS | aarch64 | 実機（kqueue reactor） | `full-macos` | `60fc2a63` | 1197 | — | 591 | 50 |
| Windows | x86_64 | 実機（WSAPoll reactor） | `full-windows` | `60fc2a63` | 1145 | 57 | 577 | 48 |
| FreeBSD | aarch64 | QEMU + HVF（14.3） | `full-freebsd` | `60fc2a63` | 1223 | 63 | 591 | 50 |
| FreeBSD | x86_64 | QEMU + KVM（14.3） | `full-freebsd` | `df6b5aae` | 1223 | 63 | 591 | 50 |
| OpenBSD | aarch64 | QEMU + HVF（7.9） | `full-openbsd` | `60fc2a63` | 1193 | 63 | 590（無視 1） | 49 |
| OpenBSD | x86_64 | QEMU + KVM（7.9） | `full-openbsd` | `df6b5aae` | 1193 | 63 | 590（無視 1） | 49（B-358・B-359 の後） |
| NetBSD | aarch64 | QEMU + HVF（10.1） | `full-netbsd` | `60fc2a63` | 1194 | 63 | 591 | 49 |
| NetBSD | x86_64 | QEMU + KVM（10.1） | `full-netbsd` | `df6b5aae` | 1194 | 63 | 591 | 49 |

最終の結果はすべて失敗 0。途中で落ちて回し直したものと、その原因は下の注のとおり（NetBSD x86_64 の 1 件は原因を確かめられていない）。

- 件数の違いは、feature とプラットフォームの `cfg` による（Windows は Unix 専用の試験、BSD は `full-*` に含まれない機能の試験が対象外）。
- BSD の 3 OS は両アーキで OS 固有のサンドボックス（FreeBSD の capsicum・OpenBSD の pledge/unveil・NetBSD の chroot + 特権降格）の下での
  E2E（`bsd-vm.sh <os> <arch> security-e2e`）も通った（aarch64 は `60fc2a63`、x86_64 は `df6b5aae`）。
- ライブラリ（`tests/api/library_e2e.sh all`）: Linux x86_64（`df6b5aae`）は 955 項目すべて ok。Windows（`60fc2a63`）も全部通った。
- io_uring の `offload` の取りこぼし（B-354）: 読み捨ての前の提出を外すと、順を確定させた `a_drain_keeps_the_signal_of_a_wait_still_in_the_sq` が
  `a waiter never woke up` で落ちることを確かめてから、修正を元に戻して全項目を回した。

注:

- Linux x86_64 の自己完結型 E2E（`full`）の 1 回目は、Proxy-Wasm の 3 件（`b186`・`b188`・`b189`）が `wait_for_port`（30 秒）で落ちた。別の作業の
  負荷（load average 11。全体で 499 秒かかった。普段は 50 秒ほど）のためで、回し直しで 52 件すべて通った（54 秒）。
- NetBSD x86_64 の 1 回目は 2 件落ちて、回し直しでは再現しなかった（単体 + 統合を 1 回、E2E を 2 回、それぞれ全部通った）。
  - 単体の `large_responses_on_pooled_connection_replenish_window`（Proxy-Wasm の gRPC の 5 秒の期限切れ）。v0.11.0 の x86_64 NetBSD でも同じ種類の試験が VM の負荷で落ちている。単独で 5 回続けて通った（0.02 秒）。
  - E2E の `test_wasm_header_operations`（応答に Wasm のヘッダが無い）。プロキシのログでは、この要求の要求ヘッダのコールバックと終了の間に 1.8 秒の空白があり、応答ヘッダのコールバックが動いていない。Wasm の 1 回のコールバックの実時間の上限（約 1 秒）に当たった可能性があるが、失敗のログは抑制されていて（同じ場所の直前の失敗で）確かめられていない。
- OpenBSD x86_64 の自己完結型 E2E の 1 回目は 2 件落ちて、どちらも試験の側の問題だったので直してから回し直した（49 件すべて成功）。
  - `f199_vds_grpc_stream_tls`: 試験用の制御プレーンが AWS-LC を使っていて、OpenBSD x86_64 の実行専用の `.text` の中の定数表を読んで SIGSEGV になっていた（[B-358](../backlog/bugs/B-358-vds-server-aws-lc-openbsd-xonly.md)）。
  - `b172_route_cache_keeps_earlier_header_and_query_routes`: 試験のプロセスの fd の上限が OpenBSD の既定（ソフト 128）のままで `EMFILE`（[B-359](../backlog/bugs/B-359-standalone-e2e-nofile-limit.md)）。
  - この 2 つを直した後に自己完結型 E2E を回し直した結果はすべて成功（Linux x86_64 の `full`・`full,epoll` が 52 件、macOS・FreeBSD x86_64 が 50 件、OpenBSD・NetBSD の両アーキが 49 件）。上の表の件数と同じ。
- Windows のライブラリの E2E の失敗は [B-352](../backlog/bugs/B-352-free-port-tcp-only.md)（`free_port` が TCP しか見ず、aofish の UDP だけの
  予約範囲に当たった）で、試験のポートの選び方を直して通った。

### ビルド・静的解析（Linux x86_64、`bb9b4fec`。`df6b5aae` との `src/` の差なし）

| 対象 | 結果 |
|---|---|
| `cargo clippy -D warnings`: `default`・`--no-default-features`・`full`・`full,epoll`・`full-container`・`full,api`・`full,handler,mesh`・各 feature の単独（`--no-default-features --features <f>`）・ワークスペースの各クレート・`veil-proxy-kit` の各 feature（`docs/artifacts/v0120/warnings/sweep.sh`、68 件） | 警告・指摘 0（`--features aio` だけは build.rs が FreeBSD 以外で意図して止める） |
| macOS（`full-macos`）・Windows（`full-windows`）の clippy `-D warnings`（`60fc2a63`） | 指摘 0 |
| `cargo fmt --check` | 差分なし |
| `#[allow(dead_code)]` | 追加なし |
| `tools/lint/check-messages.py`・`tools/release/check-docs.sh` | 0 件 |

### セキュリティ

`tools/container_security/` を 2 段で回した（aofish の Docker、`df6b5aae` のイメージ）。どちらも失敗 0。

- 既定のフェーズ（`veil:v0120-epoll`。公開イメージと同じ reactor のビルド）: 28 passed / 0 failed / 8 skipped。`cargo_audit`・`cargo_deny`・`trivy` を含む
  （`cargo_audit` は [B-356](../backlog/bugs/B-356-audit-libsqlite3-sys-test-crate.md) の理由付きの除外を含む）。
- 拡張のフェーズ（`veil:v0120-uring` と、ソースからの ASAN/TSAN のビルド）: 29 passed / 0 failed / 7 skipped。
- 2 段のスキップは互いの実行で埋まる（既定のフェーズの `libfuzzer_asan`・`libfuzzer_tsan`・`bad_backend`・`pumba`・`resource_exhaustion`・`zap` は拡張のフェーズで、
  拡張のフェーズの `libfuzzer`・`testssl`・`semgrep`・`sbom`・`gitleaks` は既定のフェーズで回っている）。両方でスキップなのは次の 2 つだけ。
  - `differential`（nginx との応答の差分。専用の構成が要り、既定でスキップ）。
  - `kernel`（試験ではなく、実行環境の io_uring・kTLS の有無の記録。レポートの `skip_reason=` の文字でスキップに数えられる）。

### 計測の側の不具合

- [B-355](../backlog/bugs/B-355-run-client-timeout-sig-proxy.md): 計測クライアントの打ち切り（`timeout docker run`）が効かず、[B-354](../backlog/bugs/B-354-offload-drain-loses-queued-poll.md) の
  止まった要求でスイートが 13 時間止まっていた。`timeout -k 10` と打ち切りの扱いを直した。
- [B-357](../backlog/bugs/B-357-wrk-rps-wall-clock-jump.md): wrk の `Requests/sec` は壁時計の差で割るので、aofish の WSL2 の時計の戻りが計測の窓に入ると rps が高く出る。
  要求数を設定の秒数で割るように直し、これ以降の wrk の行は修正後のパーサで取り直した。

## 静的配信のキャッシュの修正の交互 A/B（B-348・B-349・B-354）

応答の版を揃える修正（[B-348](../backlog/bugs/B-348-fast-path-drops-open-file-cache-fd.md)・[B-349](../backlog/bugs/B-349-content-cache-version-mismatch.md)）は静的配信の経路に
手を入れる（キャッシュに当たるときに増えるのは版の比較 2 つと、offload の中の fd の引き継ぎの判定だけ）。これと、
[B-354](../backlog/bugs/B-354-offload-drain-loses-queued-poll.md) の修正が offload の多い経路の費用にならないことを、`proxy_ab.py`（aofish、`--profile dist`、
5 ラウンドの交互）で確かめた。すべてエラー 0。基準は修正の前のコミット `c9689c23`。

### Linux x86_64、最終コミット（io_uring、`df6b5aae`）

new/base の rps の比（CPU/要求の増減を括弧内に）。HTTP/1.1 の行（wrk）は、壁時計の戻り（[B-357](../backlog/bugs/B-357-wrk-rps-wall-clock-jump.md)）が入った行（`wrk_timeouts` が 0 より大きい）を除いた中央値。

| 経路 | キャッシュあり（`open_file_cache` + `static_file_cache`）3B | 54KB | キャッシュ無し（`--no-file-caches`。要求ごとに offload）3B | 54KB |
|---|---|---|---|---|
| HTTP/1.1 | 1.045（12.30 → 11.71 µs） | 1.011 | 1.014（37.57 → 36.96 µs） | 1.019 |
| HTTP/1.1 + TLS | 1.017 | 1.028 | 1.054 | 1.029（65.10 → 62.16 µs） |
| h2c | 0.975 | 1.005 | 1.036 | 1.048 |
| HTTP/2 | 0.995 | 1.003 | 1.043 | 1.022 |
| HTTP/3 | 0.984 | 0.992 | 1.004（50.24 → 50.04 µs） | 0.994 |

- 待機者ごとに SQ を流して読み捨てる最初の修正（`60fc2a63`）では、キャッシュ無しの HTTP/3 の 3B が 0.931（CPU/要求 50.83 → 52.43 µs）だった。1 周回に 1 回の
  読み捨て（`bb9b4fec`）で 1.004 に戻った（[B-354](../backlog/bugs/B-354-offload-drain-loses-queued-poll.md)）。
- epoll（B-354 の修正の対象外。`60fc2a63`）: キャッシュあり 0.974〜1.024、キャッシュ無し 0.975〜1.088。

### FreeBSD 14.3 aarch64（`c9689c23` → `84750079`、`version_ab_bsd.sh`）

| 経路 | 3B（rps） | 54KB（rps） |
|---|---|---|
| HTTP/1.1 平文（`h1_file_plain`） | 380,633 → 374,944 | 61,995 → 62,210 |
| HTTP/1.1 + TLS（`h1_file_tls`） | 297,536 → 301,194 | 35,246 → 35,857 |
| HTTP/2（`h2_file_tls`） | 541,982 → 545,658 | 33,888 → 32,503 |
| h2c（`h2c_file_plain`） | 557,542 → 550,944 | 66,537 → 65,456 |
| HTTP/3（`h3_file`） | 168,512 → 170,311 | 3,864 → 3,838 |

VM の揺れの範囲で、版の照合による退行は見えない。
