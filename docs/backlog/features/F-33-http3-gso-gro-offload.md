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

## 対応状況（追記）: 受信側 GRO 配線 + 受信/送信ホットパスのゼロアロケーション化

送信側 GSO に続き、**受信側 GRO を HTTP/3 受信ループへ配線**した。あわせて受信・送信
ホットパスのヒープ確保とコピーを排除した。

### 受信側（`src/http3_server.rs` メインループ）

- 受信を `socket.recv_from`（io_uring RECV、データグラム毎に所有権付き `Vec` を要求）から
  `socket.recv_gro_async`（`recvmsg(2)` + `UDP_GRO` CMSG）へ切替え。同一フローの複数 QUIC
  データグラムを **カーネルで集約受信**し、per-datagram の syscall を削減する。
- 受信バッファ（64KB）を **loop 外で一度だけ確保して再利用**。旧実装はデータグラム毎に
  `vec![0u8; 65536]` を確保し、さらに `recv_buf[..len].to_vec()` と `pkt_buf.to_vec()` の
  **2 回のコピー**を行っていた。新実装は GRO セグメントを `recv_buf` のスライスとして
  `quiche::Header::from_slice` と `conn.recv` に**直接渡す**ため、データグラム毎の
  **3 回のヒープ確保 + 2 回のコピーをすべて排除**（ゼロコピー受信）。
- GRO セグメント長（`gro_segment_size`）で受信バッファを分割し、各データグラムを quiche に
  供給する。GRO 非対応カーネルでは cmsg が付かず `seg_size = total`（単発データグラム）と
  なるため、**コード分岐なしで安全にフォールバック**する。
- ソケットは `SOCK_NONBLOCK`。EAGAIN 時は `wait_readable_fd`（POLL_ADD）で待機し、
  **io_uring の新規オペコードは増やさない**。`timeout` で包んでも `ReadableFd` の `Drop` が
  POLL_ADD を `detach_op` でキャンセルするため安全（B-07）。

### 送信側（`send_pending_packets` / `flush_gso_batch`）

- 単一パケット送信で `batch[..].to_vec()` していた箇所を、新規 `QuicUdpSocket::send_to_slice_async`
  （`&[u8]` をそのまま非ブロッキング `sendto`、EAGAIN 時 POLL_ADD 待機）に置換し、
  **パケット毎の `to_vec` を排除**。最も高頻度な単一パケット経路でゼロアロケーション化。
- `send_pending_packets` の送信スクラッチ（`send_buf` 1350B + GSO 連結バッファ `batch` +
  境界 `offsets`）を **スレッドローカルから払い出して再利用**（thread-per-core のためロック不要）。
  旧実装は呼び出し（≒ループ 1 周あたり 2 回）ごとに `vec![0u8; 1350]` と `batch`/`offsets` を
  確保していた。`take`/`put` 方式で `.await` をまたいでスレッドローカルの borrow を保持せず
  再入安全。

### 検証

- `cargo check --features "http3,http2"` / `--features full` 通過。
- E2E（features full）の HTTP/3 テストが回帰なし。
- GRO による syscall 削減効果の定量計測は GRO 対応カーネル/NIC が必要なため本環境では未実施。
  非対応環境でも単発データグラム受信として正しく動作することは E2E で確認。

### 残（任意・大規模）

GRO で集約したデータグラムを quiche の `recv` に 1 回でまとめて渡す（quiche 側の
バッチ recv API はないため現状はセグメント毎 recv）最適化、および GSO/GRO のセグメント
サイズ自動調整は、効果が環境依存かつ quiche の API 制約があるため任意の継続課題とする。
