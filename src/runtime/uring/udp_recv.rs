//! HTTP/3 UDP 受信: パイプライン化 `IORING_OP_RECVMSG`（F-130 C1）
//!
//! quiche は sans-IO のため I/O は本モジュールが担う。F-129 までは「先頭 1 データグラムを
//! `IORING_OP_RECVMSG` 単発で受け、継続 drain は同期 `recvmmsg`」という二階建てだった。
//! 本実装（F-130 C1）は継続 drain も含めてホットパスから libc `recvmmsg` を排除し、
//! **常に `batch` 個の `IORING_OP_RECVMSG` を in-flight に保つソフトウェアパイプライン**
//! （§14.5 フェーズ1a 相当）に置き換える。
//!
//! ## なぜ真の Multishot（`IORING_RECV_MULTISHOT`）ではないか
//!
//! カーネルの `IORING_RECV_MULTISHOT` + provided buffers は multi-peer の unconnected UDP
//! では peer アドレスを per-CQE で安全に取り出すのが難しく（`io_uring_recvmsg_out` の
//! 埋め込みレイアウト実装が必要）、ENOBUFS 耐性も運用上シビアになる（F-129 の既知課題）。
//! 本実装は **N 個の独立 `IORING_OP_RECVMSG` を同時に in-flight へ積む**ことで、真の
//! multishot と同等の「1 回の待ちで複数データグラムを回収する」効果を、既存の安定した
//! 単発 RECVMSG + per-slot msghdr の枠組みのまま達成する（真 multishot は C2 として
//! 引き続き将来検討）。
//!
//! ## アルゴリズム
//!
//! 1. `new()` で `batch` 個のスロット（各々が独立した msghdr/iovec/addr/cmsg/buf を
//!    ワーカー起動時に 1 回だけ確保）を作り、全スロットへ RECVMSG を **1 回の submit** で
//!    まとめて投げる。
//! 2. `recv_batch()` は「1 件以上のスロットが完了する」まで待つ Future。完了済みスロットが
//!    あれば即座に個数を返す（複数同時完了も 1 回の poll でまとめて拾う = drain 相当）。
//! 3. 呼び出し側は `ready_slot(i)` / `take_result(i)` / `payload_mut(i, len)` で各データグラムを
//!    処理し、処理し終えたら `rearm_ready()` を呼んで消費済みスロットへ新しい RECVMSG を
//!    **1 回の submit** でまとめて再投入する。
//!
//! これにより「受信 syscall（recvmmsg）を毎回同期発行する」経路が消え、待ち・データ取得が
//! すべて io_uring CQE 経由になる。EAGAIN 待ち専用の `POLL_ADD` はフォールバック
//! （`recv_gro_async`、`VEIL_H3_MULTISHOT=0` または reactor ビルド）に限定される。
//!
//! ## ホットパス規則
//!
//! - 受信バッファ / msghdr / cmsg はワーカー起動時に 1 回だけ確保し再利用（スロットあたり）
//! - ペイロードはバッファ内スライスを quiche へ直渡し（ディープコピーなし）
//! - `ready` インデックス配列も起動時に確保済み（drain のたびの Vec 確保なし）
//! - 待機は io_uring CQE のみ

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::os::unix::io::RawFd;
use std::pin::Pin;
use std::task::{Context, Poll};

use super::executor::{
    alloc_op, detach_op, remove_op, set_op_waker, submit_sqes, take_op_result, with_ring, OpGuard,
};
use super::ring::{IORING_OP_RECVMSG, IORING_RECVSEND_POLL_FIRST};

const NAME_CAP: usize = std::mem::size_of::<libc::sockaddr_storage>();
const CMSG_CAP: usize = 128;
const PAYLOAD_CAP: usize = 65536;

/// 1 スロット分の完了メタ情報（`from`/GRO セグメント長/ペイロード長）。
#[derive(Debug, Clone, Copy)]
pub struct SlotMeta {
    pub from: SocketAddr,
    pub gro_segment_size: Option<u16>,
    pub payload_len: usize,
}

/// 受信スロット 1 本。msghdr/iovec/addr/cmsg/buf を Box で固定アドレス化し、in-flight 中に
/// カーネルが参照するポインタが移動しないようにする（`MmsgRecvScratch` と同方針）。
struct RecvSlot {
    buf: Box<[u8]>,
    addr: Box<libc::sockaddr_storage>,
    #[allow(dead_code)] // 理由: FFI 生ポインタ（msg.msg_control）のバッキングストア
    cmsg: Box<[u8]>,
    msg: Box<libc::msghdr>,
    #[allow(dead_code)] // 理由: FFI 生ポインタ（msg.msg_iov）のバッキングストア
    iov: Box<libc::iovec>,
    /// 現在 in-flight な op の user_data（未提出時は 0）。
    user_data: u64,
    /// SQE 提出済みで CQE 未取得か。
    submitted: bool,
    /// 完了済みで未消費の結果（Ok=メタ情報 / Err=recvmsg エラー）。
    result: Option<io::Result<SlotMeta>>,
}

impl RecvSlot {
    fn new() -> Self {
        let mut buf = vec![0u8; PAYLOAD_CAP].into_boxed_slice();
        let mut addr: Box<libc::sockaddr_storage> = Box::new(unsafe { std::mem::zeroed() });
        let mut cmsg = vec![0u8; CMSG_CAP].into_boxed_slice();
        let mut iov: Box<libc::iovec> = Box::new(libc::iovec {
            iov_base: buf.as_mut_ptr() as *mut libc::c_void,
            iov_len: PAYLOAD_CAP,
        });
        let mut msg: Box<libc::msghdr> = Box::new(unsafe { std::mem::zeroed() });
        msg.msg_name = addr.as_mut() as *mut _ as *mut libc::c_void;
        msg.msg_namelen = NAME_CAP as libc::socklen_t;
        msg.msg_iov = iov.as_mut() as *mut libc::iovec;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = CMSG_CAP as _;
        msg.msg_flags = 0;

        Self {
            buf,
            addr,
            cmsg,
            msg,
            iov,
            user_data: 0,
            submitted: false,
            result: None,
        }
    }

    /// カーネルが書き換えるフィールドをリセットして SQE を積む（submit はしない）。
    fn arm_no_submit(&mut self, fd: RawFd) -> io::Result<()> {
        debug_assert!(!self.submitted);
        self.msg.msg_namelen = NAME_CAP as libc::socklen_t;
        self.msg.msg_controllen = CMSG_CAP as _;
        self.msg.msg_flags = 0;
        self.iov.iov_base = self.buf.as_mut_ptr() as *mut libc::c_void;
        self.iov.iov_len = PAYLOAD_CAP;
        self.msg.msg_iov = self.iov.as_mut() as *mut libc::iovec;
        self.msg.msg_name = self.addr.as_mut() as *mut _ as *mut libc::c_void;
        self.msg.msg_control = self.cmsg.as_mut_ptr() as *mut libc::c_void;

        let user_data = alloc_op();
        let msg_ptr = self.msg.as_ref() as *const libc::msghdr as u64;
        let acquired = with_ring(|ring| {
            if let Some(sqe) = ring.get_sqe_or_submit() {
                sqe.opcode = IORING_OP_RECVMSG;
                sqe.fd = fd;
                sqe.addr_or_splice_off_in = msg_ptr;
                sqe.len = 1;
                // 空ソケット想定で初回同期試行を飛ばし、内部 poll から開始。
                sqe.ioprio = IORING_RECVSEND_POLL_FIRST;
                sqe.op_flags = 0;
                sqe.user_data = user_data;
                true
            } else {
                false
            }
        });
        if !acquired {
            remove_op(user_data);
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        self.user_data = user_data;
        self.submitted = true;
        Ok(())
    }
}

/// パイプライン化 `IORING_OP_RECVMSG` による UDP 受信セッション（1 ソケット / 1 ワーカー）。
///
/// 常に `batch` 個の RECVMSG を in-flight に保つことで、libc `recvmmsg` によるまとめ取りと
/// 同等のスループットを io_uring CQE 経由で達成する（F-130 C1）。
pub struct PipelinedUdpRecv {
    fd: RawFd,
    slots: Box<[RecvSlot]>,
    /// 直近の `recv_batch()` で完了が見つかったスロット index。
    /// 起動時に `batch` 容量で確保済み（drain のたびの確保なし）。
    ready: Box<[u32]>,
    ready_len: usize,
}

impl PipelinedUdpRecv {
    /// `batch` 個のスロットを確保し、初期 RECVMSG を全スロットへ **1 回の submit** で投入する。
    pub fn new(fd: RawFd, batch: usize) -> io::Result<Self> {
        let batch = batch.clamp(1, 128);
        let mut slots: Vec<RecvSlot> = (0..batch).map(|_| RecvSlot::new()).collect();
        for slot in slots.iter_mut() {
            slot.arm_no_submit(fd)?;
        }
        submit_sqes()?;
        Ok(Self {
            fd,
            slots: slots.into_boxed_slice(),
            ready: vec![0u32; batch].into_boxed_slice(),
            ready_len: 0,
        })
    }

    #[inline]
    pub fn batch_size(&self) -> usize {
        self.slots.len()
    }

    /// 1 件以上のデータグラムが完了するまで待つ Future。完了は複数同時に見つかることがあり、
    /// その場合は 1 回の poll でまとめて `ready` へ積む（`Ok(n)` で件数を返す）。
    pub fn recv_batch(&mut self) -> RecvBatch<'_> {
        RecvBatch { inner: self }
    }

    /// 非ブロッキングでスキャンし、完了済みスロットを `ready` へ積む。件数を返す。
    fn scan(&mut self) -> usize {
        self.ready_len = 0;
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if !slot.submitted {
                continue;
            }
            let Some(res) = take_op_result(slot.user_data) else {
                continue;
            };
            slot.submitted = false;
            slot.user_data = 0;
            slot.result = Some(build_slot_result(res, &slot.addr, &slot.msg));
            self.ready[self.ready_len] = i as u32;
            self.ready_len += 1;
        }
        self.ready_len
    }

    /// 直近の `recv_batch()` 完了で見つかったスロット数。
    #[inline]
    pub fn ready_count(&self) -> usize {
        self.ready_len
    }

    /// `ready` の i 番目のスロット index を返す（0 <= i < ready_count()）。
    #[inline]
    pub fn ready_slot(&self, i: usize) -> usize {
        self.ready[i] as usize
    }

    /// 指定スロットの完了結果を取り出す（1 回のみ消費可能）。
    pub fn take_result(&mut self, idx: usize) -> io::Result<SlotMeta> {
        self.slots[idx]
            .result
            .take()
            .expect("take_result called on slot without a pending result")
    }

    /// 指定スロットのペイロード（可変。quiche recv / Header::from_slice 用）。
    pub fn payload_mut(&mut self, idx: usize, len: usize) -> &mut [u8] {
        &mut self.slots[idx].buf[..len.min(PAYLOAD_CAP)]
    }

    /// 直近 `recv_batch()` で消費した ready スロットへ RECVMSG を再アームし、
    /// **1 回の submit** でまとめて投入する。呼び出し側は `ready` の全件を処理し終えた後に
    /// 呼ぶこと（未処理のまま呼ぶと `result` が上書きされずに残っている分は次回 scan まで
    /// 無視される点に注意 — 通常フローでは常に処理直後に呼ばれる）。
    pub fn rearm_ready(&mut self) -> io::Result<()> {
        for i in 0..self.ready_len {
            let idx = self.ready[i] as usize;
            let slot = &mut self.slots[idx];
            if !slot.submitted {
                slot.arm_no_submit(self.fd)?;
            }
        }
        self.ready_len = 0;
        submit_sqes()
    }
}

fn build_slot_result(
    res: i32,
    addr: &libc::sockaddr_storage,
    msg: &libc::msghdr,
) -> io::Result<SlotMeta> {
    if res < 0 {
        return Err(io::Error::from_raw_os_error(-res));
    }
    let len = res as usize;
    if len > PAYLOAD_CAP {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "recvmsg length exceeds buffer",
        ));
    }
    let from = raw_to_socket_addr(addr)?;
    let gro = parse_gro_cmsg(msg);
    Ok(SlotMeta {
        from,
        gro_segment_size: gro,
        payload_len: len,
    })
}

impl Drop for PipelinedUdpRecv {
    fn drop(&mut self) {
        // F-129 から踏襲: ワーカー終了時のみ発生する経路であり、リング/エグゼキュータも
        // 同時に破棄されるため OpGuard::Noop で後始末不要とする（バッファ解放は行わない）。
        for slot in self.slots.iter_mut() {
            if slot.submitted && slot.user_data != 0 {
                detach_op(slot.user_data, OpGuard::Noop);
                slot.submitted = false;
            }
        }
    }
}

/// `recv_batch` Future。
pub struct RecvBatch<'a> {
    inner: &'a mut PipelinedUdpRecv,
}

impl Future for RecvBatch<'_> {
    type Output = io::Result<usize>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self.inner;

        let n = this.scan();
        if n > 0 {
            return Poll::Ready(Ok(n));
        }

        // 完了なし: in-flight 中の全スロットへ waker を登録する。いずれか 1 つでも完了すれば
        // 次回 poll でまとめて拾う（複数完了も 1 回の poll で drain される）。
        let mut any_in_flight = false;
        for slot in this.slots.iter() {
            if slot.submitted {
                set_op_waker(slot.user_data, cx.waker().clone());
                any_in_flight = true;
            }
        }
        if !any_in_flight {
            // 呼び出し側の不変条件違反（rearm し忘れ）。runtime バグとして検出できるよう
            // エラーを返す（無限 Pending でハングさせない）。
            return Poll::Ready(Err(io::Error::other(
                "PipelinedUdpRecv: no slots in flight and none ready (missing rearm_ready?)",
            )));
        }
        Poll::Pending
    }
}

fn raw_to_socket_addr(storage: &libc::sockaddr_storage) -> io::Result<SocketAddr> {
    match storage.ss_family as libc::c_int {
        libc::AF_INET => {
            let sin = storage as *const _ as *const libc::sockaddr_in;
            let sin_ref = unsafe { &*sin };
            let ip = std::net::Ipv4Addr::from(sin_ref.sin_addr.s_addr.to_ne_bytes());
            let port = u16::from_be(sin_ref.sin_port);
            Ok(SocketAddr::V4(std::net::SocketAddrV4::new(ip, port)))
        }
        libc::AF_INET6 => {
            let sin6 = storage as *const _ as *const libc::sockaddr_in6;
            let sin6_ref = unsafe { &*sin6 };
            let ip = std::net::Ipv6Addr::from(sin6_ref.sin6_addr.s6_addr);
            let port = u16::from_be(sin6_ref.sin6_port);
            Ok(SocketAddr::V6(std::net::SocketAddrV6::new(
                ip,
                port,
                sin6_ref.sin6_flowinfo,
                sin6_ref.sin6_scope_id,
            )))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Unknown address family",
        )),
    }
}

fn parse_gro_cmsg(msg: &libc::msghdr) -> Option<u16> {
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(msg) };
    while !cmsg.is_null() {
        let cmsg_ref = unsafe { &*cmsg };
        if cmsg_ref.cmsg_level == libc::SOL_UDP && cmsg_ref.cmsg_type == libc::UDP_GRO {
            let data_ptr = unsafe { libc::CMSG_DATA(cmsg) as *const u16 };
            return Some(unsafe { *data_ptr });
        }
        cmsg = unsafe { libc::CMSG_NXTHDR(msg, cmsg) };
    }
    None
}
