//! HTTP/3 UDP 受信: `IORING_OP_RECVMSG` + `IORING_RECV_MULTISHOT`（F-124）
//!
//! quiche は sans-IO のため I/O は本モジュールが担う。従来の `POLL_ADD` + 同期
//! `recvmsg`/`recvmmsg` 二重往復を廃し、io_uring multishot RECVMSG + provided
//! buffers でデータグラムを非同期に取り出す。
//!
//! ## バッファレイアウト（kernel multishot recvmsg）
//!
//! 各 provided buffer（提出時 `msghdr.msg_namelen` / `msg_controllen` が領域幅）:
//! ```text
//! [io_uring_recvmsg_out 16B][name 領域][control 領域][payload]
//! ```
//! CQE.res はバッファに書き込まれた総バイト数。payload 長は header 内 `payloadlen`。
//!
//! ## ホットパス規則
//!
//! - 提供バッファはワーカー起動時に 1 回だけ確保し再利用
//! - ペイロードはバッファ内スライスをそのまま quiche へ渡す（ディープコピーなし）
//! - 待機は io_uring CQE のみ（同期 recvmmsg をホットパスに置かない）

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::os::unix::io::RawFd;
use std::pin::Pin;
use std::task::{Context, Poll};

use super::executor::{
    alloc_multishot_op, alloc_op, detach_op, detach_op_no_cancel, poll_completions, remove_op,
    set_op_waker, submit_sqes, take_multishot_cqe, take_op_result, wait_for_completions, with_ring,
    OpGuard,
};
use super::ring::{
    IORING_CQE_BUFFER_SHIFT, IORING_CQE_F_BUFFER, IORING_OP_PROVIDE_BUFFERS, IORING_OP_RECVMSG,
    IORING_OP_REMOVE_BUFFERS, IORING_RECVSEND_POLL_FIRST, IORING_RECV_MULTISHOT,
    IOSQE_BUFFER_SELECT,
};

/// `struct io_uring_recvmsg_out`（kernel ABI、16 バイト）
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct IoUringRecvmsgOut {
    namelen: u32,
    controllen: u32,
    payloadlen: u32,
    flags: u32,
}

const RECVMSG_OUT_SIZE: usize = std::mem::size_of::<IoUringRecvmsgOut>();
const NAME_CAP: usize = std::mem::size_of::<libc::sockaddr_storage>();
const CMSG_CAP: usize = 128;
/// GRO 集約を含む 1 データグラム上限。
const PAYLOAD_CAP: usize = 65536;
/// 1 provided buffer のストライド。
const BUF_STRIDE: usize = RECVMSG_OUT_SIZE + NAME_CAP + CMSG_CAP + PAYLOAD_CAP;
/// provided buffer group id（ワーカー内で一意）。
const BGID: u16 = 1;

/// Multishot 受信で取り出した 1 データグラム。
#[derive(Debug)]
pub struct MultishotDatagram {
    pub from: SocketAddr,
    pub gro_segment_size: Option<u16>,
    bid: u16,
    payload_off: usize,
    payload_len: usize,
}

impl MultishotDatagram {
    #[inline]
    pub fn bid(&self) -> u16 {
        self.bid
    }
}

/// Multishot UDP 受信セッション（1 UDP ソケット / 1 ワーカー）。
pub struct MultishotUdpRecv {
    fd: RawFd,
    batch: usize,
    buffers: Box<[u8]>,
    /// multishot 提出用 msghdr（namelen/controllen のみ意味を持つ）。
    msg: Box<libc::msghdr>,
    to_reprovide: Vec<u16>,
    user_data: u64,
    submitted: bool,
    need_rearm: bool,
    ready: VecDeque<MultishotDatagram>,
    buffers_registered: bool,
}

impl MultishotUdpRecv {
    /// `batch` 個の provided buffers を確保しカーネルへ登録する（コールドパス）。
    pub fn new(fd: RawFd, batch: usize) -> io::Result<Self> {
        let batch = batch.clamp(1, 128);
        let buffers = vec![0u8; batch * BUF_STRIDE].into_boxed_slice();
        let mut msg: Box<libc::msghdr> = Box::new(unsafe { std::mem::zeroed() });
        msg.msg_namelen = NAME_CAP as libc::socklen_t;
        msg.msg_controllen = CMSG_CAP as _;
        msg.msg_iov = std::ptr::null_mut();
        msg.msg_iovlen = 0;

        let mut s = Self {
            fd,
            batch,
            buffers,
            msg,
            to_reprovide: Vec::with_capacity(batch),
            user_data: 0,
            submitted: false,
            need_rearm: true,
            ready: VecDeque::with_capacity(batch),
            buffers_registered: false,
        };
        s.provide_all_buffers()?;
        Ok(s)
    }

    /// 全バッファを provided buffer pool に一括登録する。
    fn provide_all_buffers(&mut self) -> io::Result<()> {
        let addr = self.buffers.as_mut_ptr() as u64;
        let user_data = alloc_op();
        let fd_nr = self.batch as i32;
        let acquired = with_ring(|ring| {
            if let Some(sqe) = ring.get_sqe_or_submit() {
                sqe.opcode = IORING_OP_PROVIDE_BUFFERS;
                sqe.fd = fd_nr;
                sqe.addr_or_splice_off_in = addr;
                sqe.len = BUF_STRIDE as u32;
                sqe.off_or_addr2 = 0;
                sqe.buf_index_or_buf_group = BGID;
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
        submit_sqes()?;
        // コールドパス: 完了まで待機。
        loop {
            poll_completions();
            if let Some(res) = take_op_result(user_data) {
                if res < 0 {
                    return Err(io::Error::from_raw_os_error(-res));
                }
                break;
            }
            wait_for_completions()?;
        }
        self.buffers_registered = true;
        self.need_rearm = true;
        Ok(())
    }

    /// 消費済み bid を再提供する。
    fn reprovide_pending(&mut self) {
        if self.to_reprovide.is_empty() {
            return;
        }
        let mut i = 0;
        while i < self.to_reprovide.len() {
            let bid = self.to_reprovide[i];
            let addr = unsafe { self.buffers.as_mut_ptr().add(bid as usize * BUF_STRIDE) as u64 };
            let user_data = alloc_op();
            let acquired = with_ring(|ring| {
                if let Some(sqe) = ring.get_sqe_or_submit() {
                    sqe.opcode = IORING_OP_PROVIDE_BUFFERS;
                    sqe.fd = 1;
                    sqe.addr_or_splice_off_in = addr;
                    sqe.len = BUF_STRIDE as u32;
                    sqe.off_or_addr2 = bid as u64;
                    sqe.buf_index_or_buf_group = BGID;
                    sqe.user_data = user_data;
                    true
                } else {
                    false
                }
            });
            if !acquired {
                remove_op(user_data);
                break;
            }
            // fire-and-forget: キャンセルせず CQE 到着でスロット解放（PROVIDE を中断しない）。
            detach_op_no_cancel(user_data, OpGuard::Noop);
            i += 1;
        }
        let _ = submit_sqes();
        self.to_reprovide.drain(..i);
    }

    /// Multishot RECVMSG を arm する。
    fn arm_multishot(&mut self) -> io::Result<()> {
        if self.submitted && !self.need_rearm {
            return Ok(());
        }
        if self.submitted && self.user_data != 0 {
            // 旧 multishot をキャンセル（スロット解放）。
            detach_op(self.user_data, OpGuard::Noop);
            self.submitted = false;
            self.user_data = 0;
        }

        let user_data = alloc_multishot_op();
        let fd = self.fd;
        let msg_ptr = self.msg.as_ref() as *const libc::msghdr as u64;
        let acquired = with_ring(|ring| {
            if let Some(sqe) = ring.get_sqe_or_submit() {
                sqe.opcode = IORING_OP_RECVMSG;
                sqe.fd = fd;
                sqe.addr_or_splice_off_in = msg_ptr;
                sqe.len = 1;
                sqe.ioprio = IORING_RECV_MULTISHOT | IORING_RECVSEND_POLL_FIRST;
                sqe.flags = IOSQE_BUFFER_SELECT;
                sqe.buf_index_or_buf_group = BGID;
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
        submit_sqes()?;
        self.user_data = user_data;
        self.submitted = true;
        self.need_rearm = false;
        Ok(())
    }

    /// Multishot CQE を ready キューへ変換する。
    fn drain_cqes_to_ready(&mut self) {
        if !self.submitted || self.user_data == 0 {
            return;
        }
        let ud = self.user_data;
        loop {
            let (item, finished) = take_multishot_cqe(ud);
            match item {
                Some((res, flags)) => {
                    if res < 0 {
                        if res == -libc::ENOBUFS {
                            // バッファ枯渇: 全再提供を試みる（コールド寄り）。
                            let _ = self.provide_all_buffers();
                        }
                        // エラー CQE で multishot 終端。
                        self.need_rearm = true;
                        if finished {
                            self.submitted = false;
                            self.user_data = 0;
                        }
                        continue;
                    }
                    if (flags & IORING_CQE_F_BUFFER) == 0 {
                        continue;
                    }
                    let bid = ((flags >> IORING_CQE_BUFFER_SHIFT) & 0xffff) as u16;
                    match self.parse_buffer(bid, res as usize) {
                        Some(dg) => self.ready.push_back(dg),
                        None => self.to_reprovide.push(bid),
                    }
                    if finished {
                        // この CQE が最終。スロットはキュー空なら解放済み。
                        self.need_rearm = true;
                        // 追加の take で解放確認。
                        let (more, fin) = take_multishot_cqe(ud);
                        if more.is_none() && fin {
                            self.submitted = false;
                            self.user_data = 0;
                        }
                    }
                }
                None => {
                    if finished {
                        self.submitted = false;
                        self.user_data = 0;
                        self.need_rearm = true;
                    }
                    break;
                }
            }
            if !self.submitted {
                break;
            }
        }
    }

    /// provided buffer を解析する。`buf_len` は CQE.res（書き込み総バイト）。
    fn parse_buffer(&self, bid: u16, buf_len: usize) -> Option<MultishotDatagram> {
        let base = bid as usize * BUF_STRIDE;
        if base + RECVMSG_OUT_SIZE > self.buffers.len() {
            return None;
        }
        let header = RECVMSG_OUT_SIZE + NAME_CAP + CMSG_CAP;
        if buf_len < header {
            return None;
        }
        let buf = &self.buffers[base..base + BUF_STRIDE.min(buf_len.max(header))];
        let out = unsafe { &*(buf.as_ptr() as *const IoUringRecvmsgOut) };
        let name_len = (out.namelen as usize).min(NAME_CAP);
        let ctrl_len = (out.controllen as usize).min(CMSG_CAP);
        let payload_len = out.payloadlen as usize;
        if payload_len > PAYLOAD_CAP || header + payload_len > BUF_STRIDE {
            return None;
        }
        if buf_len < header + payload_len {
            return None;
        }

        let name_off = RECVMSG_OUT_SIZE;
        let ctrl_off = name_off + NAME_CAP;
        let payload_off = ctrl_off + CMSG_CAP;

        let name_bytes = &self.buffers[base + name_off..base + name_off + name_len.max(2)];
        let from = parse_sockaddr(name_bytes)?;
        let gro = parse_gro_from_cmsg(&self.buffers[base + ctrl_off..base + ctrl_off + ctrl_len]);

        Some(MultishotDatagram {
            from,
            gro_segment_size: gro,
            bid,
            payload_off: base + payload_off,
            payload_len,
        })
    }

    /// ペイロード（不変）。
    pub fn payload<'a>(&'a self, dg: &MultishotDatagram) -> &'a [u8] {
        &self.buffers[dg.payload_off..dg.payload_off + dg.payload_len]
    }

    /// ペイロード（可変。quiche recv / Header::from_slice 用）。
    pub fn payload_mut<'a>(&'a mut self, dg: &MultishotDatagram) -> &'a mut [u8] {
        let start = dg.payload_off;
        let end = start + dg.payload_len;
        &mut self.buffers[start..end]
    }

    /// 処理済みデータグラムの bid を再提供キューへ戻す。
    pub fn release(&mut self, dg: &MultishotDatagram) {
        self.to_reprovide.push(dg.bid);
        self.reprovide_pending();
    }

    /// 1 データグラムを待つ Future。
    pub fn recv_one(&mut self) -> RecvOne<'_> {
        RecvOne { inner: self }
    }

    /// 既に ready なら取り出す（非ブロッキング）。
    pub fn try_recv(&mut self) -> Option<MultishotDatagram> {
        self.drain_cqes_to_ready();
        self.ready.pop_front()
    }

    /// バッチ幅。
    #[inline]
    pub fn batch_size(&self) -> usize {
        self.batch
    }
}

impl Drop for MultishotUdpRecv {
    fn drop(&mut self) {
        if self.submitted && self.user_data != 0 {
            detach_op(self.user_data, OpGuard::Noop);
            self.submitted = false;
        }
        if self.buffers_registered {
            let user_data = alloc_op();
            let nr = self.batch as i32;
            let acquired = with_ring(|ring| {
                if let Some(sqe) = ring.get_sqe_or_submit() {
                    sqe.opcode = IORING_OP_REMOVE_BUFFERS;
                    sqe.fd = nr;
                    sqe.buf_index_or_buf_group = BGID;
                    sqe.user_data = user_data;
                    true
                } else {
                    false
                }
            });
            if acquired {
                let _ = submit_sqes();
                detach_op(user_data, OpGuard::Noop);
            } else {
                remove_op(user_data);
            }
        }
    }
}

/// `MultishotUdpRecv::recv_one` の Future。
pub struct RecvOne<'a> {
    inner: &'a mut MultishotUdpRecv,
}

impl Future for RecvOne<'_> {
    type Output = io::Result<MultishotDatagram>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self.inner;

        this.reprovide_pending();
        this.drain_cqes_to_ready();
        if let Some(dg) = this.ready.pop_front() {
            return Poll::Ready(Ok(dg));
        }

        if this.need_rearm || !this.submitted {
            if let Err(e) = this.arm_multishot() {
                return Poll::Ready(Err(e));
            }
        }

        this.drain_cqes_to_ready();
        if let Some(dg) = this.ready.pop_front() {
            return Poll::Ready(Ok(dg));
        }

        if this.user_data != 0 {
            set_op_waker(this.user_data, cx.waker().clone());
        }
        Poll::Pending
    }
}

fn parse_sockaddr(name: &[u8]) -> Option<SocketAddr> {
    if name.len() < std::mem::size_of::<libc::sa_family_t>() {
        return None;
    }
    let family = unsafe { std::ptr::read_unaligned(name.as_ptr() as *const libc::sa_family_t) };
    match family as i32 {
        libc::AF_INET if name.len() >= std::mem::size_of::<libc::sockaddr_in>() => {
            let sin = unsafe { &*(name.as_ptr() as *const libc::sockaddr_in) };
            let ip = std::net::Ipv4Addr::from(sin.sin_addr.s_addr.to_ne_bytes());
            let port = u16::from_be(sin.sin_port);
            Some(SocketAddr::V4(std::net::SocketAddrV4::new(ip, port)))
        }
        libc::AF_INET6 if name.len() >= std::mem::size_of::<libc::sockaddr_in6>() => {
            let sin6 = unsafe { &*(name.as_ptr() as *const libc::sockaddr_in6) };
            let ip = std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr);
            let port = u16::from_be(sin6.sin6_port);
            Some(SocketAddr::V6(std::net::SocketAddrV6::new(
                ip,
                port,
                sin6.sin6_flowinfo,
                sin6.sin6_scope_id,
            )))
        }
        _ => None,
    }
}

fn parse_gro_from_cmsg(ctrl: &[u8]) -> Option<u16> {
    if ctrl.is_empty() {
        return None;
    }
    let mut off = 0usize;
    while off + std::mem::size_of::<libc::cmsghdr>() <= ctrl.len() {
        let cmsg = unsafe { &*(ctrl.as_ptr().add(off) as *const libc::cmsghdr) };
        let cmsg_len = cmsg.cmsg_len;
        if cmsg_len < std::mem::size_of::<libc::cmsghdr>() || off + cmsg_len > ctrl.len() {
            break;
        }
        if cmsg.cmsg_level == libc::SOL_UDP && cmsg.cmsg_type == libc::UDP_GRO {
            // CMSG_DATA オフセット（cmsghdr 直後、アライン済み）。
            let data_off = unsafe {
                (libc::CMSG_DATA(ctrl.as_ptr().add(off) as *mut libc::cmsghdr) as usize)
                    - (ctrl.as_ptr() as usize)
                    - off
            };
            if off + data_off + 2 <= ctrl.len() {
                let mut b = [0u8; 2];
                b.copy_from_slice(&ctrl[off + data_off..off + data_off + 2]);
                return Some(u16::from_ne_bytes(b));
            }
        }
        let align = std::mem::size_of::<usize>();
        let next = (cmsg_len + align - 1) & !(align - 1);
        if next == 0 {
            break;
        }
        off += next;
    }
    None
}
