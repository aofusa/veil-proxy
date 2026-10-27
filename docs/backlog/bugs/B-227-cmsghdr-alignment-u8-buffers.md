# B-227: `cmsghdr` を `u8` のバッファの先頭にキャストして読み書きしていて、8 バイトの整列を保証していない

**状態: 修正済み（v0.10.0）**

## 出典

2026-10-27 のセキュリティレビュー（v0.9.2 の `src/runtime/uring/udp_recv.rs`・`udp_send.rs`・`src/udp/socket.rs` の読解）。

## 事象

- `RecvSlot::cmsg: Box<[u8]>`・`SendSlot::cmsg`・`udp/socket.rs::build_gso_cmsg` の `[u8; N]` を `*mut libc::cmsghdr` にキャストして
  `cmsg_len`・`cmsg_level`・`cmsg_type` を書く。`u8` のスライスの整列の保証は 1 で、`cmsghdr` は 8 バイトの整列を要求する。
- 今のアロケータ（mimalloc・veil-alloc・glibc）は実際には 8 以上の整列を返すので動いているが、Rust の規則上は未定義動作で、Miri・
  将来のアロケータの変更で壊れ得る。

## 改修案

- `#[repr(C, align(8))] struct CmsgBuf([u8; N])` のラッパ型を導入して、`Box<CmsgBuf>`・スタックの `CmsgBuf` で持つ。読み出しは `read_unaligned`
  のままでよい（`parse_recvmsg_buf` はすでに `read_unaligned`）。
- `build_gso_cmsg`・`parse_gro_cmsg` は純粋関数なので Miri で動かして確かめる。

## 受け入れ条件

- `cargo miri test` で `build_gso_cmsg`・`parse_gro_cmsg` の単体試験が通る。HTTP/3 の E2E（GSO・GRO）が変わらない。

## 修正（v0.10.0）

- `build_gso_cmsg` はヘッダと u16 のデータを `write_unaligned` で書く。
- 解析は新しい純粋関数 `udp::socket::find_gro_segment(&[u8])` に寄せ、`CMSG_FIRSTHDR`/`CMSG_NXTHDR`（ヘッダを参照として読む）を使わずに `read_unaligned` で走査する。長さの壊れたヘッダで打ち切り、UDP_GRO のデータはカーネルの形（int）と u16 の両方を受ける。`udp::socket` と io_uring の受信（スロット・提供バッファ）の両方がこれを使う。
- 試験: `test_gso_cmsg_roundtrip_at_unaligned_offsets`（0〜7 バイトずらした位置で組み立て・解析）、`test_find_gro_segment_rejects_malformed`。

## 関連

- [F-33](../features/F-33-http3-gso-gro-offload.md)（GSO・GRO）
