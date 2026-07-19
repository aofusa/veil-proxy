//! HTTP/3 UDP 受信: `IORING_OP_RECVMSG`（+ multishot 修飾、F-124）
//!
//! quiche は sans-IO のため I/O は本モジュールが担う。従来の `POLL_ADD` + 同期
//! `recvmsg` 二重往復を廃し、**1 SQE の `IORING_OP_RECVMSG`** でデータグラムを
//! 非同期受信する。
//!
//! ## Multishot について
//!
//! カーネルの `IORING_RECV_MULTISHOT` + provided buffers は multi-peer の unconnected
//! UDP では peer アドレスを per-CQE で安全に取り出すのが難しく、かつ ENOBUFS 耐性が
//! 運用上シビアになる。本実装は次の二段で「multishot 相当のスループット」を確保する:
//!
//! 1. **先頭パケット**: `IORING_OP_RECVMSG` 単発（GRO cmsg 付き、peer アドレス付き）
//! 2. **継続 drain**: 呼び出し側が `recvmmsg` で最大 `mmsg_batch_size` 件を一括掻き出し
//!
//! SQE には `IORING_RECVSEND_POLL_FIRST` を付け、空ソケット時の無駄な同期試行を避ける。
//! （真の multishot フラグは将来の buffer-ring 経路で再有効化する余地をコメントで残す。）
//!
//! ## ホットパス規則
//!
//! - 受信バッファ / msghdr / cmsg はワーカー起動時に 1 回だけ確保し再利用
//! - ペイロードはバッファ内スライスを quiche へ直渡し（ディープコピーなし）
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

/// 受信した 1 データグラム（バッファは `MultishotUdpRecv` 内に保持）。
#[derive(Debug)]
pub struct MultishotDatagram {
    pub from: SocketAddr,
    pub gro_segment_size: Option<u16>,
    payload_len: usize,
}

/// `IORING_OP_RECVMSG` ベースの UDP 受信セッション（1 ソケット / 1 ワーカー）。
pub struct MultishotUdpRecv {
    fd: RawFd,
    batch: usize,
    /// ペイロード領域。
    buf: Box<[u8]>,
    /// peer アドレス。
    addr: Box<libc::sockaddr_storage>,
    /// GRO 等の cmsg。
    cmsg: Box<[u8]>,
    /// カーネルへ渡す msghdr（固定アドレス）。
    msg: Box<libc::msghdr>,
    /// iovec（buf を指す）。
    iov: Box<libc::iovec>,
    user_data: u64,
    submitted: bool,
    /// 完了済みで未取り出しの結果。
    pending: Option<MultishotDatagram>,
}

impl MultishotUdpRecv {
    /// スクラッチを確保してポインタ配線する（コールドパス）。
    pub fn new(fd: RawFd, batch: usize) -> io::Result<Self> {
        let batch = batch.clamp(1, 128);
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

        Ok(Self {
            fd,
            batch,
            buf,
            addr,
            cmsg,
            msg,
            iov,
            user_data: 0,
            submitted: false,
            pending: None,
        })
    }

    #[inline]
    pub fn batch_size(&self) -> usize {
        self.batch
    }

    /// ペイロード（不変）。
    pub fn payload<'a>(&'a self, dg: &MultishotDatagram) -> &'a [u8] {
        &self.buf[..dg.payload_len]
    }

    /// ペイロード（可変。quiche recv / Header::from_slice 用）。
    pub fn payload_mut<'a>(&'a mut self, dg: &MultishotDatagram) -> &'a mut [u8] {
        &mut self.buf[..dg.payload_len]
    }

    /// 処理済み通知（単発バッファ再利用のため no-op。API 互換）。
    pub fn release(&mut self, _dg: &MultishotDatagram) {}

    /// 非ブロッキングで未取り出し結果があれば返す（単発経路では通常 None）。
    pub fn try_recv(&mut self) -> Option<MultishotDatagram> {
        self.pending.take()
    }

    /// 1 データグラムを待つ Future。
    pub fn recv_one(&mut self) -> RecvOne<'_> {
        RecvOne { inner: self }
    }

    fn arm(&mut self) -> io::Result<()> {
        if self.submitted {
            return Ok(());
        }
        // カーネルが書き換えるフィールドをリセット。
        self.msg.msg_namelen = NAME_CAP as libc::socklen_t;
        self.msg.msg_controllen = CMSG_CAP as _;
        self.msg.msg_flags = 0;
        self.iov.iov_base = self.buf.as_mut_ptr() as *mut libc::c_void;
        self.iov.iov_len = PAYLOAD_CAP;
        self.msg.msg_iov = self.iov.as_mut() as *mut libc::iovec;
        self.msg.msg_name = self.addr.as_mut() as *mut _ as *mut libc::c_void;
        self.msg.msg_control = self.cmsg.as_mut_ptr() as *mut libc::c_void;

        let user_data = alloc_op();
        let fd = self.fd;
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
        submit_sqes()?;
        self.user_data = user_data;
        self.submitted = true;
        Ok(())
    }

    fn take_completion(&mut self) -> Option<io::Result<MultishotDatagram>> {
        if !self.submitted {
            return None;
        }
        let res = take_op_result(self.user_data)?;
        self.submitted = false;
        self.user_data = 0;
        if res < 0 {
            return Some(Err(io::Error::from_raw_os_error(-res)));
        }
        let len = res as usize;
        if len > PAYLOAD_CAP {
            return Some(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "recvmsg length exceeds buffer",
            )));
        }
        let from = match raw_to_socket_addr(self.addr.as_ref()) {
            Ok(a) => a,
            Err(e) => return Some(Err(e)),
        };
        let gro = parse_gro_cmsg(self.msg.as_ref());
        Some(Ok(MultishotDatagram {
            from,
            gro_segment_size: gro,
            payload_len: len,
        }))
    }
}

impl Drop for MultishotUdpRecv {
    fn drop(&mut self) {
        if self.submitted && self.user_data != 0 {
            detach_op(self.user_data, OpGuard::Noop);
            self.submitted = false;
        }
    }
}

/// `recv_one` Future。
pub struct RecvOne<'a> {
    inner: &'a mut MultishotUdpRecv,
}

impl Future for RecvOne<'_> {
    type Output = io::Result<MultishotDatagram>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self.inner;

        if let Some(p) = this.pending.take() {
            return Poll::Ready(Ok(p));
        }

        if let Some(r) = this.take_completion() {
            return Poll::Ready(r);
        }

        if !this.submitted {
            if let Err(e) = this.arm() {
                return Poll::Ready(Err(e));
            }
        }

        if let Some(r) = this.take_completion() {
            return Poll::Ready(r);
        }

        if this.user_data != 0 {
            set_op_waker(this.user_data, cx.waker().clone());
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
