//! HTTP/3 UDP 送信: パイプライン化 `IORING_OP_SENDMSG`（F-130 C3）
//!
//! F-129 までの送信経路は libc `sendmmsg`（`QuicUdpSocket::send_mmsg_async`）で、EAGAIN 時は
//! `POLL_ADD` で書き込み可能を待ってから同期 `sendmmsg` を再試行していた。本実装は送信も
//! ホットパスから libc `sendmmsg` を排除し、エントリごとに 1 SQE の `IORING_OP_SENDMSG`
//! （GSO は `UDP_SEGMENT` cmsg を msghdr に付与）を作り、**複数 SQE を 1 回の
//! `io_uring_enter`（submit）でまとめて投入**した後、全 CQE の完了を待つ。
//!
//! `sendmmsg` の「書き込み不可なら EAGAIN でブロックし、可能になったら再試行」という
//! 待ちのセマンティクスは、io_uring では **SQE 提出時点でカーネルが内部的に poll + 再試行**
//! するため、通常はユーザー空間へ EAGAIN が戻ってこない（`send`/`sendmsg` の非同期系
//! opcode の標準動作）。よって `wait_writable_fd` への依存を排除できる。万一 CQE が負の
//! res を返した場合（EAGAIN 含む）は、既存の sendmmsg 経路と同じく **当該エントリのみ
//! skip して継続**する（QUIC は失われたパケットを自身のロスリカバリで再送するため、
//! 1 データグラム相当の送信断念はプロトコル上安全）。
//!
//! ## ホットパス規則
//!
//! - msghdr / iovec / sockaddr / cmsg はワーカー起動時に `batch` 本だけ確保し再利用
//! - ペイロードは呼び出し側スクラッチ（連結済み batch バッファ）を指すのみでコピーしない
//! - 待機は io_uring CQE のみ（`POLL_ADD` は使わない）

use std::future::Future;
use std::io;
use std::os::unix::io::RawFd;
use std::pin::Pin;
use std::task::{Context, Poll};

use ftlog::warn;

use crate::udp::socket::{build_gso_cmsg, socket_addr_to_raw, SendmmsgEntry};

use super::executor::{
    alloc_op, cancel_op, detach_op, poll_completions, remove_op, set_op_waker, submit_sqes,
    take_op_result, with_ring, OpGuard,
};
use super::ring::IORING_OP_SENDMSG;

const CMSG_CAP: usize = 128;

/// 送信スロット 1 本。msghdr/iovec/addr/cmsg を Box で固定アドレス化する。
/// `iov_base` は呼び出し側スクラッチ（`send_batch` の `entries[i].data`）を指すため、
/// in-flight 中は呼び出し側がそのバッファを生かしたまま `send_batch().await` を保持する
/// 契約（呼び出し元 `send_pending_packets` は sweep 全体でスクラッチを所有し続ける）。
struct SendSlot {
    addr: Box<libc::sockaddr_storage>,
    cmsg: Box<[u8]>,
    iov: Box<libc::iovec>,
    msg: Box<libc::msghdr>,
    user_data: u64,
    submitted: bool,
}

impl SendSlot {
    fn new() -> Self {
        let mut addr: Box<libc::sockaddr_storage> = Box::new(unsafe { std::mem::zeroed() });
        let mut cmsg = vec![0u8; CMSG_CAP].into_boxed_slice();
        let mut iov: Box<libc::iovec> = Box::new(unsafe { std::mem::zeroed() });
        let mut msg: Box<libc::msghdr> = Box::new(unsafe { std::mem::zeroed() });
        msg.msg_name = addr.as_mut() as *mut _ as *mut libc::c_void;
        msg.msg_iov = iov.as_mut() as *mut libc::iovec;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = 0;
        msg.msg_flags = 0;
        Self {
            addr,
            cmsg,
            iov,
            msg,
            user_data: 0,
            submitted: false,
        }
    }

    /// エントリ内容を msghdr へ書き込む（確保なし）。GSO は `segments > 1` の場合のみ
    /// `UDP_SEGMENT` cmsg を付ける（`segments == 1` は cmsg なし、既存 sendmmsg 経路と同じ）。
    fn configure(&mut self, entry: &SendmmsgEntry<'_>) -> io::Result<()> {
        let (sockaddr, sockaddr_len) = socket_addr_to_raw(entry.dest);
        *self.addr = sockaddr;
        // SAFETY: entry.data はホットパス契約上、送信完了（CQE 到着）まで呼び出し側が
        // 生存させる連結済み送信バッファのスライス。SENDMSG は読み取り専用に使うため
        // *mut へのキャストは安全（カーネルは書き込まない）。
        self.iov.iov_base = entry.data.as_ptr() as *mut libc::c_void;
        self.iov.iov_len = entry.data.len();
        self.msg.msg_iov = self.iov.as_mut() as *mut libc::iovec;
        self.msg.msg_name = self.addr.as_mut() as *mut _ as *mut libc::c_void;
        self.msg.msg_namelen = sockaddr_len;
        if entry.segments > 1 {
            let cmsg_len = build_gso_cmsg(&mut self.cmsg, entry.seg_size)?;
            self.msg.msg_control = self.cmsg.as_mut_ptr() as *mut libc::c_void;
            self.msg.msg_controllen = cmsg_len as _;
        } else {
            self.msg.msg_controllen = 0;
        }
        self.msg.msg_flags = 0;
        Ok(())
    }

    /// in-flight の op を、msghdr・iovec・アドレス・cmsg の Box を後始末の閉包へ移して detach する。スロットには
    /// 新しい Box を入れる（終了・失敗の経路だけなので確保は問題にならない。B-225）。
    fn detach_keep_alive(&mut self) {
        if !self.submitted || self.user_data == 0 {
            return;
        }
        let fresh = SendSlot::new();
        let keep = (
            std::mem::replace(&mut self.addr, fresh.addr),
            std::mem::replace(&mut self.cmsg, fresh.cmsg),
            std::mem::replace(&mut self.iov, fresh.iov),
            std::mem::replace(&mut self.msg, fresh.msg),
        );
        detach_op(
            self.user_data,
            OpGuard::Cleanup(Box::new(move |_res| drop(keep))),
        );
        self.user_data = 0;
        self.submitted = false;
    }

    /// SQE を積む（submit はしない）。
    fn submit_no_flush(&mut self, fd: RawFd) -> io::Result<()> {
        // B-225: release では debug_assert が消え、in-flight のスロットを上書きすると前の op の user_data を失って
        // op テーブルのスロットが漏れた（カーネルが読んでいる msghdr を書き換えることにもなる）
        if self.submitted {
            return Err(io::Error::from(io::ErrorKind::WouldBlock));
        }
        let user_data = alloc_op();
        let msg_ptr = self.msg.as_ref() as *const libc::msghdr as u64;
        let acquired = with_ring(|ring| {
            if let Some(sqe) = ring.get_sqe_or_submit() {
                sqe.opcode = IORING_OP_SENDMSG;
                sqe.fd = fd;
                sqe.addr_or_splice_off_in = msg_ptr;
                sqe.len = 1;
                sqe.ioprio = 0;
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

/// パイプライン化 `IORING_OP_SENDMSG` による UDP 送信セッション（1 ソケット / 1 ワーカー）。
pub struct UringUdpSend {
    fd: RawFd,
    slots: Box<[SendSlot]>,
}

impl UringUdpSend {
    /// `batch` 本のスロットを確保する（コールドパス）。
    pub fn new(fd: RawFd, batch: usize) -> Self {
        let batch = batch.clamp(1, 128);
        let slots: Vec<SendSlot> = (0..batch).map(|_| SendSlot::new()).collect();
        Self {
            fd,
            slots: slots.into_boxed_slice(),
        }
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// `entries`（`capacity()` 以下）をまとめて `IORING_OP_SENDMSG` で送出し、全件の完了
    /// （CQE 到着）を待つ。1 回の `submit_sqes()` で複数 SQE をまとめて `io_uring_enter` する。
    /// 個々のエントリが失敗しても他エントリの送信は継続する（skip + warn、既存 sendmmsg
    /// フォールバックと同じ「恒久エラーは 1 メッセージだけ諦める」方針）。
    pub fn send_batch<'a>(&'a mut self, entries: &'a [SendmmsgEntry<'a>]) -> SendBatch<'a> {
        SendBatch {
            inner: self,
            entries,
            armed: false,
            remaining: 0,
        }
    }
}

impl Drop for UringUdpSend {
    fn drop(&mut self) {
        // B-225: in-flight の SENDMSG はカーネルが msghdr 等を読み終えるまで Box を生かす（以前は OpGuard::Noop で
        // すぐ解放していた）。通常は SendBatch の Drop が完了まで待つので、ここで残っているのは期限切れのものだけ。
        for slot in self.slots.iter_mut() {
            slot.detach_keep_alive();
        }
    }
}

// B-225: 送るデータ（iov_base）は呼び出し側のバッファ（HTTP/3 の送信のスクラッチ）を指す。完了前に SendBatch が
// 落とされる（終了時に親のタスクごと drop される）と、そのバッファも解放され、カーネルが解放済みのメモリを読んで
// 相手へ送り得た。Drop で取り消しを投げ、CQE が来るまで（上限 1 秒）その場で待つ。UDP の送信は取り消しも含めて
// すぐ終わる。期限を過ぎたものは msghdr 等を生かして detach する（データ自体は守れないが、到達しない想定）。
impl Drop for SendBatch<'_> {
    fn drop(&mut self) {
        if !self.armed || self.remaining == 0 {
            return;
        }
        for slot in self.inner.slots.iter() {
            if slot.submitted {
                cancel_op(slot.user_data);
            }
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            for slot in self.inner.slots.iter_mut() {
                if slot.submitted && take_op_result(slot.user_data).is_some() {
                    slot.submitted = false;
                    slot.user_data = 0;
                }
            }
            if !self.inner.slots.iter().any(|s| s.submitted)
                || std::time::Instant::now() >= deadline
            {
                break;
            }
            // 理由: 終了時・取り消し時だけ通る。取り消しの CQE は直ちに来る
            let _ = with_ring(|ring| ring.submit_and_wait(1));
            poll_completions();
        }
        for slot in self.inner.slots.iter_mut() {
            slot.detach_keep_alive();
        }
    }
}

/// [`UringUdpSend::send_batch`] の Future。
pub struct SendBatch<'a> {
    inner: &'a mut UringUdpSend,
    entries: &'a [SendmmsgEntry<'a>],
    armed: bool,
    remaining: usize,
}

impl Future for SendBatch<'_> {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;

        if !this.armed {
            let count = this.entries.len().min(this.inner.slots.len());
            if count == 0 {
                return Poll::Ready(Ok(()));
            }
            for i in 0..count {
                if let Err(e) = this.inner.slots[i].configure(&this.entries[i]) {
                    // cmsg 構築失敗（バッファ不足など、実運用では起こらない想定）は
                    // 当該エントリを skip して残りは通常どおり送る。
                    warn!("[HTTP/3] sendmsg entry configure skipped: {}", e);
                    continue;
                }
                if let Err(e) = this.inner.slots[i].submit_no_flush(this.inner.fd) {
                    warn!("[HTTP/3] sendmsg entry submit skipped: {}", e);
                    continue;
                }
                this.remaining += 1;
            }
            if let Err(e) = submit_sqes() {
                return Poll::Ready(Err(e));
            }
            this.armed = true;
            if this.remaining == 0 {
                return Poll::Ready(Ok(()));
            }
        }

        loop {
            let mut progressed = false;
            for slot in this.inner.slots.iter_mut() {
                if !slot.submitted {
                    continue;
                }
                if let Some(res) = take_op_result(slot.user_data) {
                    slot.submitted = false;
                    slot.user_data = 0;
                    this.remaining -= 1;
                    progressed = true;
                    if res < 0 {
                        let err = io::Error::from_raw_os_error(-res);
                        // EMSGSIZE は DF を立てた経路 MTU の探索で大きすぎた探索のパケット（想定内。quiche が下げる）
                        if err.raw_os_error() == Some(libc::EMSGSIZE) {
                            ftlog::debug!("[HTTP/3] sendmsg entry skipped: {}", err);
                        } else {
                            warn!("[HTTP/3] sendmsg entry skipped: {}", err);
                        }
                    }
                }
            }
            if this.remaining == 0 {
                return Poll::Ready(Ok(()));
            }
            if !progressed {
                for slot in this.inner.slots.iter() {
                    if slot.submitted {
                        set_op_waker(slot.user_data, cx.waker().clone());
                    }
                }
                return Poll::Pending;
            }
        }
    }
}
