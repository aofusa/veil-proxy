# F-124: HTTP/3 quiche 輻輳制御・バッチ設定 + IORING_OP_RECVMSG 受信経路

**ステータス: 完了（2026-07-19）— ただし真 Multishot は未達。後続は [F-125](F-125-http3-quiche-multishot-followup.md)**

---

## 機能説明（実施内容）

1. **`[http3]` で quiche `Config` の輻輳制御 / Pacing / HyStart を設定可能**
   - `cc_algorithm`（既定 `bbr`）、`pacing`（既定 true）、`max_pacing_rate`、`hystart`（既定 true）
2. **UDP mmsg バッチ幅を既定 64 にし、設定可能**
   - `mmsg_batch_size`（1..=128）
3. **受信待機を `IORING_OP_RECVMSG`（単発 + `POLL_FIRST`）へ**し、継続は `recvmmsg` drain
   - 当初目標だった **`IORING_RECV_MULTISHOT` + provided buffers は試作後に後退**（multi-peer UDP で不安定）
   - 詳細・残り作業は **F-125 を正** とする

## 改修内容

- `src/config.rs` / `Http3ServerConfig` / `entry.rs`（TOML → ワーカー配線修正含む）
- `src/udp/socket.rs`（batch 既定 64・`with_batch`）
- `src/runtime/uring/udp_recv.rs`（RECVMSG 単発 Future）
- `src/runtime/uring/executor.rs` / `ring.rs`（RECVMSG 許可。multishot OpTable は残骸として残存 → F-125）
- `src/http3_server.rs`（CC 適用・受信分岐）
- `examples/config.toml` / `contrib/config/config.toml` / README / README.ja

## 受け入れ条件（F-124 時点で満たしたもの）

- [x] CC/Pacing/batch の TOML 設定
- [x] 既定 batch 64
- [x] POLL_ADD+同期 recvmsg の先頭待機を IORING_OP_RECVMSG に置換
- [x] 主要 build / unit test / E2E おおむね通過、Docker GSO on/off
- [ ] ~~真 Multishot~~ → **F-125**
- [ ] ~~AGENTS.md / コメント整合 / tools/perf 公式~~ → **F-125**

## 依存・リスク

- 真 Multishot 未実装。命名（`MultishotUdpRecv` / `VEIL_H3_MULTISHOT`）が実態とずれる
- executor に未使用の multishot 完了キュー・PROVIDE 許可が残る（security surface）

## 関連

- 後続: [F-125](F-125-http3-quiche-multishot-followup.md)（**委譲用フル引き継ぎドキュメント**）
- 前提: [F-115](F-115-http3-recvmmsg-sendmmsg-batching.md)、[F-33](F-33-http3-gso-gro-offload.md)
