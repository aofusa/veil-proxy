# F-281: 2026-10 のセキュリティレビューで読めなかった領域の続き（veil_alloc・kTLS の受信・本文の転送・BSD/macOS のサンドボックス・reactor・Wasm の外部呼び出し・メッシュの mTLS/RBAC）

**状態: 完了（2026-10-27 起票、同日に読了）**

## 結果（2026-10-27）

8 領域を読み、本文の転送は aofish の Docker Desktop で v0.9.2 を使って実機再現した（`docs/artifacts/security/analysis/07-f281-followup.md`・
`docs/artifacts/security/repro/`、git 管理外）。GraphQL（`src/api/graphql`）はこのリリース線に無いので対象外。

- 起票: [B-244](../bugs/B-244-h1-chunked-timeout-upstream-conn-pooled-mid-response.md)（P1、chunked の停止で応答の途中の上流接続がプールに戻る）、
  [B-245](../bugs/B-245-h1-head-204-304-upstream-body-wait.md)、[B-246](../bugs/B-246-upstream-response-cl-te-precedence.md)、
  [B-247](../bugs/B-247-wasm-dequeue-shared-queue-abi-writes-payload-not-pointer.md)、[B-248](../bugs/B-248-wasi-random-get-not-csprng.md)、
  [B-249](../bugs/B-249-xds-rbac-unsupported-rules-fail-open.md)、[B-250](../bugs/B-250-sandbox-capabilities-bounding-set-only.md)、
  [B-251](../bugs/B-251-upstream-connect-failure-error-log-per-request.md)、Low/Info は [F-282](F-282-security-review-followup-hardening.md)。
- 欠陥なし: `src/veil_alloc/`（全 `unsafe`）、reactor（`WakerSlot`・Future の Drop・readiness 確認）、`sendfile.rs`/`splice.rs` の TLS ガード、
  `negotiation.rs`、`l4/proxy.rs`、`config/validate.rs`、capsicum/pledge/unveil/chroot/Job object。
- 残り（次回）: `grpc_executor.rs` の本体、`mesh/identity.rs` の後半、KT-02 の実機再現。

## 今の状態

2026-10-27 のレビュー（v0.9.2 のコード読解。io_uring ランタイム・ゼロコピー経路・seccomp/Landlock・HTTP/1.1/2/3 のパーサ・Wasm の ABI・
C API・connect_broker・JWT・ACME・VDS・依存関係・CI）で読めなかった領域。優先順に:

1. `src/veil_alloc/`（2421 行）: `realloc` の Kind 跨ぎ（small→large）、`free_remote`（別スレッドの解放）の順序付け、`drain_remote` の ABA、
   pthread キーのデストラクタでのヒープの引き継ぎ。`alloc_soak.sh`・`leak_check.sh` で経験的に検証済みだが、競合は負荷試験では出にくい。
2. `src/ktls_rustls.rs` の受信（`fill_plaintext_nonblocking`、kTLS の制御メッセージ = alert/handshake の cmsg の扱い。`TLS_GET_RECORD_TYPE` を無視して
   平文として扱っていないか）、`src/proxy/forward.rs`（1490 行）・`transfer.rs` の本文の転送（CL と実際の長さの不一致、HEAD/204/304 の本文、
   `Content-Length` が複数の上流の応答）。
3. `src/security.rs` の namespace・mount・capability（L1835〜）と FreeBSD capsicum・OpenBSD pledge・macOS Seatbelt（L2385〜）。
   `report_security_status` が「全スレッドに適用済み」と誤って表示していないか（B-219）。
4. `src/runtime/reactor/tcp/unix.rs`（1930 行）・`kqueue.rs`・`windows.rs`: コンテナ（`full-container` = epoll）の本番経路。`WakerSlot` の同一 fd の
   複数の待機者の起床の取りこぼし。
5. `src/wasm/host/grpc_executor.rs`（1844 行）・`http_call.rs` のタイムアウト・同時実行数・応答サイズの上限、`shared_data.rs`・`shared_queue.rs` の無制限の
   成長、`wasi.rs` の露出範囲、`src/api/graphql`（深さ・複雑度の上限・introspection の既定）、`crates/veil-python`（GIL・例外の越境）。
6. `src/mesh/`（13K 行）: `xds/rbac.rs`・`linkerd/identity.rs` の mTLS の同一性の検査と RBAC。
7. `src/proxy/websocket.rs`（101 の後のトンネル）、`src/l4/proxy.rs`、`src/protocol/negotiation.rs`、`src/proxy/splice.rs`・`sendfile.rs` の
   「TLS の経路に通さない」ガード（F-155/F-240。AGENTS.md の記述だけで未検証）。
8. `src/config/` の TOML の検証（サイズ・タイムアウト・パスの上限）。

## 改修内容

- 上の順に読み、所見はチケットに起票する。方法は前回と同じ（全 `unsafe` の読解、Future の drop 時のカーネルの参照の追跡、外部入力の経路の列挙）。
- `docs/guide/security.md` の「Security Audit (F-236)」の手順（クリーンな worktree で `/claude-security:claude-security`）と差分を比べる。

## 関連

- [F-236](F-236-claude-security-audit.md)、[B-219](../bugs/B-219-seccomp-landlock-worker-threads-only.md)〜[B-239](../bugs/B-239-docker-base-images-latest-curl-sh.md)
