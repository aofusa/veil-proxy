# F-33: HTTP/3 UDP GSO/GRO オフロード

## 出典

`docs/artifacts/architecture_analysis_v3.md`（UDP パケットの GSO/GRO 活用）。

## 概要

HTTP/3（quiche）の UDP 送受信で GSO（Generic Segmentation Offload）/ GRO を活用し、
パケットごとのカーネル遷移コストを削減してスループットを数倍化する。

## 現状

- `config.toml` / `[http3]` に `gso_gro_enabled` フラグが存在するが、独自 io_uring ランタイムの
  UDP 送受信（`sendmsg`/`recvmsg`）で GSO/GRO の segment size 制御が完全にはパイプライン化
  されていない。

## 改修内容

1. `src/runtime/` に `IORING_OP_SENDMSG` / `IORING_OP_RECVMSG` の Future を追加し、
   `msghdr` + `cmsg`（`UDP_SEGMENT` / `UDP_GRO`）で一括送受信する。
2. quiche の `send`/`recv` ループを、複数 QUIC パケットを 1 回の sendmsg(GSO) でまとめて送る形に変更。
   受信は GRO で集約したデータグラムを quiche に供給する。
3. `gso_gro_enabled` でランタイム切替（非対応カーネルは単発 sendmsg にフォールバック）。

## 受け入れ条件

- [x] HTTP/3 E2E が回帰なし。
- [ ] GSO 有効時に同一スループットでの sendmsg 呼び出し回数が削減される（効果測定は GSO 対応
      カーネル/NIC が必要なため本環境では未計測）。

## 依存・リスク

- カーネル/NIC の GSO/GRO 対応差異が大きく、フォールバック設計が必須。

## 対応状況: GSO 送信バッチングを配線

GSO/GRO の下回り（`src/udp/socket.rs` の `send_with_gso_sync`（sendmsg + `UDP_SEGMENT`
CMSG）、`send_gso_async`（EAGAIN 対応・GSO 無効時は個別送信フォールバック）、
`recv_with_gro_sync` / `recv_gro_async`（`UDP_GRO`））は既に実装済みだったが、HTTP/3 の
送信ループ `send_pending_packets` では **未配線**（パケットごとに `to_vec()` でヒープ確保 →
`send_to` で 1 パケットずつ送信）だった。

- `src/http3_server.rs` の `send_pending_packets` を GSO バッチング方式に変更:
  接続ごとに **同一宛先・同一サイズ**の QUIC パケットを使い回しバッファに連結し、宛先/サイズが
  変わる・上限（64 セグメント）・最終セグメント（< segment_size）で `send_gso_async` により
  1 回の `sendmsg(UDP_SEGMENT)` で送出する。これで **パケットごとの to_vec ヒープ確保を排除**
  し、syscall 回数を削減する。
- `send_gso_async` は GSO 非対応環境では個別送信へ安全フォールバックするため、互換性は維持。
- **重要**: いずれも libc `sendmsg`（+ EAGAIN 時 `wait_writable_fd().await`）を使用しており、
  io_uring の新規オペコード追加は不要。`PROXY_ALLOWED_OPCODES` を広げず**セキュリティサーフェスを
  増やさない**。

### 検証

- E2E（features full）の HTTP/3 テストが回帰なし。
- GSO による syscall 削減効果の定量計測は GSO 対応カーネル/NIC が必要なため本環境では未実施。

### 残

受信側 GRO（`recv_gro_async`）の HTTP/3 受信ループへの配線は、quiche へ GRO セグメント分割を
渡す処理が必要なため継続課題（送信側 GSO を優先実装）。
