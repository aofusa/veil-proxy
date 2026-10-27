# F-281: 2026-10 のセキュリティレビューで読めなかった領域の続き（veil_alloc・kTLS の受信・本文の転送・BSD/macOS のサンドボックス・reactor・Wasm の外部呼び出し・メッシュの mTLS/RBAC）

**状態: 未着手（2026-10-27 起票）**

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
