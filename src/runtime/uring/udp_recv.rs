//! HTTP/3 UDP 受信: パイプライン化 `IORING_OP_RECVMSG`（F-130 C1）/
//! 真の `IORING_RECV_MULTISHOT` + provided buffer ring（F-130 C2）
//!
//! quiche は sans-IO のため I/O は本モジュールが担う。F-129 までは「先頭 1 データグラムを
//! `IORING_OP_RECVMSG` 単発で受け、継続 drain は同期 `recvmmsg`」という二階建てだった。
//!
//! ## C1: パイプライン化 RECVMSG（`PipelinedUdpRecv`）
//!
//! 継続 drain も含めてホットパスから libc `recvmmsg` を排除し、**常に `batch` 個の
//! `IORING_OP_RECVMSG` を in-flight に保つソフトウェアパイプライン**（§14.5 フェーズ1a 相当）
//! に置き換える。データグラム 1 通につき SQE 1 本が必要（真の multishot ではない）。
//!
//! ## C2: 真の Multishot + provided buffer ring（`MultishotUdpRecv`）
//!
//! `IORING_RECV_MULTISHOT`（kernel 6.0+）+ provided buffer ring
//! （`IORING_REGISTER_PBUF_RING`、kernel 5.19+）で **SQE 1 本に対して複数データグラムの
//! CQE** を受け、SQE 再投入コストを消す。peer アドレスは `io_uring_recvmsg_out` の
//! 埋め込みレイアウト（ヘッダ + name + control + payload）から復元する。`-ENOBUFS`
//! （バッファ枯渇）はエラーではなく再アームで回復する。非対応環境（buffer ring 登録失敗、
//! または実行時 `-EINVAL`）では自動的に C1 へフォールバックする（`UdpRecvBackend`）。
//! 詳細は `docs/artifacts/f130c2_multishot_bufring_design.md`。
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
use std::sync::atomic::{AtomicU16, Ordering};
use std::task::{Context, Poll};

use super::executor::{
    alloc_multishot_op, alloc_op, detach_op, remove_op, set_op_waker, submit_sqes,
    take_multishot_cqe, take_op_result, with_ring, OpGuard,
};
use super::ring::{
    IoUringBuf, IoUringRecvmsgOut, IORING_CQE_BUFFER_SHIFT, IORING_CQE_F_BUFFER, IORING_OP_RECVMSG,
    IORING_RECVSEND_POLL_FIRST, IORING_RECV_MULTISHOT, IOSQE_BUFFER_SELECT,
};

const NAME_CAP: usize = std::mem::size_of::<libc::sockaddr_storage>();
const CMSG_CAP: usize = 128;
const PAYLOAD_CAP: usize = 65536;

// ====================
// F-130 C2: 真の multishot + provided buffer ring
// ====================

/// provided buffer 1 本の先頭に置かれるヘッダのサイズ（`struct io_uring_recvmsg_out`）。
const BUF_HDR: usize = std::mem::size_of::<IoUringRecvmsgOut>();
/// provided buffer 1 本のサイズ（ヘッダ + name + control + payload）。
const BUF_SIZE: usize = BUF_HDR + NAME_CAP + CMSG_CAP + PAYLOAD_CAP;
/// provided buffer ring の 1 エントリのサイズ（`struct io_uring_buf`）。
const RING_ENTRY_SIZE: usize = std::mem::size_of::<IoUringBuf>();
/// このワーカー内で HTTP/3 UDP 受信専用に使う buffer group ID。
/// 本実装ではこの用途以外に provided buffer ring を登録しないため固定値で足りる。
const BGID: u16 = 1;
/// provided buffer ring エントリ数の上限（設計書どおり 256）。
const MAX_ENTRIES: u32 = 256;

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

    /// `ready` の i 番目のスロット index を返す（0 <= i < 直近 `recv_batch()` の戻り値）。
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

    /// パイプラインを満杯まで補充する（`recv_batch()` の全件処理後に呼ぶ）。
    ///
    /// 「未提出かつ未消費結果を持たない」スロットすべてに RECVMSG を再アームし、**1 回の
    /// submit** でまとめて投入する。直近で消費した ready スロットに加え、万一 SQ 満杯等で
    /// 過去に再アームできなかったスロットがあってもここで拾い直すため、in-flight 本数が
    /// 目減りせず自己修復する（パイプライン不変条件: 常に最大 `batch` 本を in-flight）。
    ///
    /// 呼び出し側は `ready` の全件を `take_result` で処理し終えた後に呼ぶこと。
    pub fn rearm_ready(&mut self) -> io::Result<()> {
        let fd = self.fd;
        let mut armed_any = false;
        for slot in self.slots.iter_mut() {
            if !slot.submitted && slot.result.is_none() {
                // arm 失敗（極めて稀な SQ 満杯）は当該スロットのみ諦め、次回 rearm で再挑戦する。
                if slot.arm_no_submit(fd).is_ok() {
                    armed_any = true;
                }
            }
        }
        self.ready_len = 0;
        if armed_any {
            submit_sqes()?;
        }
        Ok(())
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

/// provided buffer 1 本（ヘッダ + name + control + payload）から `SlotMeta` を切り出す純粋関数。
///
/// `buf.len() >= BUF_SIZE` を前提とする（呼び出し側が保証）。`namelen`/`controllen` が
/// バッファ作成時に要求した容量（`NAME_CAP`/`CMSG_CAP`）を超えている場合は切り詰めが
/// 発生したことを意味するため `None` を返す（呼び出し側はデータグラムを破棄し、バッファ
/// だけをリングへ返却する）。
fn parse_recvmsg_buf(buf: &[u8]) -> Option<SlotMeta> {
    debug_assert!(buf.len() >= BUF_SIZE);

    // SAFETY: buf は provided buffer 1 本分（>= BUF_SIZE）の有効な読み取り可能領域。
    // recvmsg_out ヘッダはカーネルが `IoUringRecvmsgOut` と ABI 互換のレイアウトで
    // バッファ先頭に書き込む（`ring.rs` の `recvmsg_out_struct_is_abi_compatible` で保証）。
    // アライメント保証がない（バッファは page 整列だが header サイズ 16 のみに依存）ため
    // read_unaligned を使う。
    let hdr = unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const IoUringRecvmsgOut) };

    if hdr.namelen as usize > NAME_CAP || hdr.controllen as usize > CMSG_CAP {
        // 切り詰め: name または control がバッファに収まりきらなかった。
        // 安全に復元できないためデータグラムごと破棄する。
        return None;
    }
    let payload_len = hdr.payloadlen as usize;
    if payload_len > PAYLOAD_CAP {
        // 防御的チェック（バッファ容量的に本来発生しない）。
        return None;
    }

    let name_bytes = &buf[BUF_HDR..BUF_HDR + NAME_CAP];
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    // SAFETY: コピー元 name_bytes・コピー先 storage ともに厳密に NAME_CAP
    // (= size_of::<sockaddr_storage>()) バイトの有効領域。
    unsafe {
        std::ptr::copy_nonoverlapping(
            name_bytes.as_ptr(),
            &mut storage as *mut libc::sockaddr_storage as *mut u8,
            NAME_CAP,
        );
    }
    let from = raw_to_socket_addr(&storage).ok()?;

    let control_bytes = &buf[BUF_HDR + NAME_CAP..BUF_HDR + NAME_CAP + CMSG_CAP];
    let mut tmp_msg: libc::msghdr = unsafe { std::mem::zeroed() };
    tmp_msg.msg_control = control_bytes.as_ptr() as *mut libc::c_void;
    tmp_msg.msg_controllen = hdr.controllen as _;
    let gro = parse_gro_cmsg(&tmp_msg);

    Some(SlotMeta {
        from,
        gro_segment_size: gro,
        payload_len,
    })
}

/// 真の `IORING_RECV_MULTISHOT` + provided buffer ring による UDP 受信セッション（F-130 C2）。
///
/// `PipelinedUdpRecv`（C1）が「データグラム 1 通につき SQE 1 本」だったのに対し、
/// こちらは **1 本の multishot SQE で複数データグラムの CQE** を受け取る。SQE 再投入
/// コストが原理的に消えるが、真の multishot（kernel 6.0+）と provided buffer ring
/// （kernel 5.19+）の両方に依存するため、非対応環境では `new()` が `Err` を返し、
/// 呼び出し側（`UdpRecvBackend`）が C1 へフォールバックする。
///
/// ## ホットパス規則
/// - provided buffer 領域・リング領域・msghdr はワーカー起動時に mmap で 1 回だけ確保し
///   再利用する（アドレス固定。カーネルが `bid` 経由で直接参照するため移動禁止）。
/// - バッファの返却（recycle）はリング共有メモリへの書き込み + tail の Release store のみで、
///   追加の syscall もヒープ確保も発生しない（provided buffer ring の核心的な利点）。
/// - `ready`/`results`/`pending_return` は起動時に `entries` 容量で確保済み（drain のたびの
///   確保なし）。
pub struct MultishotUdpRecv {
    fd: RawFd,
    /// provided buffer ring 領域（mmap、`entries * RING_ENTRY_SIZE` バイト）。
    ///
    /// # 不変条件
    /// カーネルは `IORING_REGISTER_PBUF_RING` 登録後、この領域を multishot RECVMSG の
    /// 生存期間中ずっと直接参照する（バッファ選択・tail 読み取り）。`Drop` で
    /// 未完了 op を detach してから解放するまで、このアドレスを絶対に動かしても
    /// 解放してもならない。
    ring_ptr: *mut u8,
    ring_size: usize,
    /// provided buffer 本体領域（mmap、`entries * BUF_SIZE` バイト）。
    ///
    /// # 不変条件
    /// カーネルは in-flight な recv 完了までこの領域内の該当バッファへ直接書き込む。
    /// `ring_ptr` と同じ寿命制約（`Drop` 参照）。
    buf_ptr: *mut u8,
    buf_region_size: usize,
    /// リングエントリ数（2 の冪）。
    entries: u32,
    /// `entries - 1`（インデックス計算用マスク）。
    mask: u32,
    bgid: u16,
    /// 次に公開する際のリング tail（カーネルの tail と同じセマンティクスで u16 wrap）。
    tail: u16,
    /// multishot RECVMSG で使い回す msghdr（1 個だけ、アドレス固定）。
    /// `msg_name`/`msg_iov` は使わない（provided buffer が受け皿になる）ため NULL。
    msg: Box<libc::msghdr>,
    /// 現在 in-flight な multishot op の user_data（0 = 未アーム）。
    user_data: u64,
    /// bid ごとの完了済み未消費結果（`entries` 容量で固定確保）。
    results: Box<[Option<SlotMeta>]>,
    /// 消費済み・破棄済みで次の `rearm_ready()` でリングへ返却すべき bid。
    /// 容量は `entries` に固定（`new()` で確保済み、毎回のヒープ確保なし）。
    pending_return: Vec<u16>,
    /// 直近の `recv_batch()` で完了が見つかった bid（`entries` 容量で固定確保）。
    ready: Box<[u32]>,
    ready_len: usize,
    /// 前回の `scan()` で検出し、まだ呼び出し側へ返していない致命的エラー
    /// （ready 分を優先して返すため 1 回遅延させる）。
    pending_error: Option<io::Error>,
    /// `-EINVAL` を観測したか（buffer ring 登録には対応するが true multishot recv 自体は
    /// 未対応というカーネルギャップ、5.19〜6.0）。呼び出し側 (`UdpRecvBackend`) が
    /// これを見て 1 度だけ C1 へダウングレードする。
    einval: bool,
}

impl MultishotUdpRecv {
    /// `batch` を 2 の冪（上限 `MAX_ENTRIES`）に切り上げた個数の provided buffer を
    /// 1 回の mmap で確保し、buffer ring として登録したうえで最初の multishot RECVMSG を
    /// アームする。
    ///
    /// 非対応環境（`IORING_REGISTER_PBUF_RING` 失敗、初回アームの失敗）では `Err` を返す。
    /// 呼び出し側はこれを見て C1（`PipelinedUdpRecv`）へフォールバックすること。
    pub fn new(fd: RawFd, batch: usize) -> io::Result<Self> {
        let entries = (batch.max(1) as u32).next_power_of_two().min(MAX_ENTRIES);
        let mask = entries - 1;

        let buf_region_size = entries as usize * BUF_SIZE;
        let ring_size = entries as usize * RING_ENTRY_SIZE;

        // SAFETY: MAP_ANONYMOUS|MAP_PRIVATE の匿名マッピング。長さは 0 より大きい
        // （entries >= 1）。失敗時は MAP_FAILED を返すのみで安全。
        let buf_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                buf_region_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if buf_ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let buf_ptr = buf_ptr as *mut u8;

        let ring_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                ring_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if ring_ptr == libc::MAP_FAILED {
            // SAFETY: buf_ptr/buf_region_size は直前に mmap した領域そのもの。
            unsafe { libc::munmap(buf_ptr as *mut libc::c_void, buf_region_size) };
            return Err(io::Error::last_os_error());
        }
        let ring_ptr = ring_ptr as *mut u8;

        if let Err(e) = with_ring(|ring| ring.register_buf_ring(BGID, entries, ring_ptr as u64)) {
            // SAFETY: 両領域とも直前に mmap した領域そのもの。登録失敗時はカーネルは
            // どちらの領域も参照しないため即座に解放してよい。
            unsafe {
                libc::munmap(ring_ptr as *mut libc::c_void, ring_size);
                libc::munmap(buf_ptr as *mut libc::c_void, buf_region_size);
            }
            return Err(e);
        }

        let mut msg: Box<libc::msghdr> = Box::new(unsafe { std::mem::zeroed() });
        msg.msg_name = std::ptr::null_mut();
        msg.msg_namelen = NAME_CAP as libc::socklen_t;
        msg.msg_iov = std::ptr::null_mut();
        msg.msg_iovlen = 0;
        msg.msg_control = std::ptr::null_mut();
        msg.msg_controllen = CMSG_CAP as _;
        msg.msg_flags = 0;

        let mut this = Self {
            fd,
            ring_ptr,
            ring_size,
            buf_ptr,
            buf_region_size,
            entries,
            mask,
            bgid: BGID,
            tail: 0,
            msg,
            user_data: 0,
            results: vec![None; entries as usize].into_boxed_slice(),
            pending_return: Vec::with_capacity(entries as usize),
            ready: vec![0u32; entries as usize].into_boxed_slice(),
            ready_len: 0,
            pending_error: None,
            einval: false,
        };

        // 全バッファを初期公開する（tail store は 1 回にまとめる）。
        for bid in 0..entries as u16 {
            this.publish_bid(bid);
        }
        this.commit_tail();

        // 初回アーム失敗時: `this.user_data` は 0 のままなので、`?` で早期 return した際に
        // `this` が自然に drop され、`Drop for MultishotUdpRecv` の「in-flight op なし」分岐が
        // unregister + munmap を行ってくれる（手動の二重解放を避ける）。
        this.arm()?;
        // 提出失敗時: SQE 自体は積んだが未提出。`this.user_data != 0` のまま drop されるため
        // `Drop` は detach_op 経由でカーネル未提出分の後始末を cleanup クロージャへ委ねる。
        submit_sqes()?;

        Ok(this)
    }

    #[inline]
    pub fn fd(&self) -> RawFd {
        self.fd
    }

    #[inline]
    pub fn batch_size(&self) -> usize {
        self.entries as usize
    }

    #[inline]
    fn buf_addr_ptr(&self, bid: u16) -> *mut u8 {
        debug_assert!((bid as u32) < self.entries);
        // SAFETY: bid < entries なので buf_ptr .. buf_ptr + buf_region_size 内に収まる。
        unsafe { self.buf_ptr.add(bid as usize * BUF_SIZE) }
    }

    /// リングエントリ 1 件を書く（tail のインクリメントのみ、公開 store は行わない）。
    /// 複数バッファをまとめて返却するときに `commit_tail()` を最後に 1 回だけ呼ぶための分割。
    fn publish_bid(&mut self, bid: u16) {
        let idx = (self.tail as u32 & self.mask) as usize;
        // SAFETY: idx < entries（mask で範囲内に収まる）。ring_ptr は entries * RING_ENTRY_SIZE
        // バイトの mmap 済み領域で、書き込み先はその範囲内。
        unsafe {
            let entry = &mut *(self.ring_ptr.add(idx * RING_ENTRY_SIZE) as *mut IoUringBuf);
            entry.addr = self.buf_addr_ptr(bid) as u64;
            entry.len = BUF_SIZE as u32;
            entry.bid = bid;
            entry.resv = 0;
        }
        self.tail = self.tail.wrapping_add(1);
    }

    /// `publish_bid` で書いたエントリをカーネルへ公開する（リング先頭エントリの `resv`
    /// フィールド = tail を Release ストア）。
    fn commit_tail(&mut self) {
        // SAFETY: ring_ptr は少なくとも RING_ENTRY_SIZE(16) バイトあり、オフセット 14 の
        // u16 は `IoUringBuf` 先頭エントリの `resv`（= カーネル ABI 上の tail）と一致する
        // （`ring.rs` の `buf_entry_resv_offset_is_14` で保証）。
        let tail_ptr = unsafe { self.ring_ptr.add(14) as *const AtomicU16 };
        unsafe { (*tail_ptr).store(self.tail, Ordering::Release) };
    }

    /// 消費・破棄済みで未返却の bid をまとめてリングへ返却する（syscall なし）。
    fn republish_pending(&mut self) {
        if self.pending_return.is_empty() {
            return;
        }
        for i in 0..self.pending_return.len() {
            let bid = self.pending_return[i];
            self.publish_bid(bid);
        }
        self.pending_return.clear();
        self.commit_tail();
    }

    /// multishot RECVMSG を 1 本アームする（`user_data == 0` のときのみ呼ぶこと）。
    fn arm(&mut self) -> io::Result<()> {
        debug_assert_eq!(self.user_data, 0);
        let user_data = alloc_multishot_op();
        let msg_ptr = self.msg.as_ref() as *const libc::msghdr as u64;
        let fd = self.fd;
        let bgid = self.bgid;
        let acquired = with_ring(|ring| {
            if let Some(sqe) = ring.get_sqe_or_submit() {
                sqe.opcode = IORING_OP_RECVMSG;
                sqe.fd = fd;
                sqe.addr_or_splice_off_in = msg_ptr;
                sqe.len = 1;
                // multishot + provided buffer select。初回同期試行はカーネルに任せる
                // （C1 と異なり POLL_FIRST は付けない: multishot は完了のたびに自動継続するため
                // 「空ソケット想定で内部pollから開始」という最適化の意味が薄く、liburing の
                // 典型例も POLL_FIRST を付けない）。
                sqe.ioprio = IORING_RECV_MULTISHOT;
                sqe.flags = IOSQE_BUFFER_SELECT;
                sqe.buf_index_or_buf_group = bgid;
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
        Ok(())
    }

    /// bid のバッファを切り出して `SlotMeta` を返す。切り詰めなど解釈不能な場合は `None`。
    fn parse_buffer(&self, bid: u16) -> Option<SlotMeta> {
        let base = self.buf_addr_ptr(bid);
        // SAFETY: base は mmap 済み provided buffer 領域内（bid < entries）で、直前の CQE で
        // カーネルがこの bid のバッファへ recvmsg_out ヘッダ + name + control + payload を
        // 書き込み済み。BUF_SIZE バイトの読み取りは領域内に収まる。
        let slice = unsafe { std::slice::from_raw_parts(base as *const u8, BUF_SIZE) };
        parse_recvmsg_buf(slice)
    }

    /// 非ブロッキングでドレインし、完了済み bid を `ready` へ積む。件数を返す。
    ///
    /// `-ENOBUFS`（バッファ枯渇）はエラーではなく無視して継続する。それ以外の負の `res` は
    /// 致命的エラーとして扱うが、同一ドレインで既に見つかった ready 分を優先して返し、
    /// エラーは `pending_error` に退避して次回呼び出しで返す（データグラムのロスを避ける）。
    fn scan(&mut self) -> io::Result<usize> {
        self.ready_len = 0;

        if let Some(e) = self.pending_error.take() {
            return Err(e);
        }
        if self.user_data == 0 {
            return Ok(0);
        }

        loop {
            let (item, finished) = take_multishot_cqe(self.user_data);
            match item {
                Some((res, flags)) => {
                    if res < 0 {
                        if res == -libc::EINVAL {
                            // buffer ring 登録には対応するが true multishot recv 自体は
                            // 未対応というカーネルギャップ（5.19〜6.0）。呼び出し側が
                            // 検出して C1 へダウングレードできるようフラグを立てる。
                            self.einval = true;
                        }
                        if res != -libc::ENOBUFS {
                            self.pending_error = Some(io::Error::from_raw_os_error(-res));
                        }
                        // ENOBUFS/EINVAL/その他エラーいずれも multishot はここで終了している
                        // （executor 側で finished=true 済み）。次のループで None を受け取って
                        // user_data==0 に落ちる。
                        continue;
                    }
                    if (flags & IORING_CQE_F_BUFFER) == 0 {
                        // buffer 未選択の異常 CQE（本来発生しないはずだが防御的に無視）。
                        continue;
                    }
                    let bid = (flags >> IORING_CQE_BUFFER_SHIFT) as u16;
                    match self.parse_buffer(bid) {
                        Some(meta) => {
                            self.results[bid as usize] = Some(meta);
                            self.ready[self.ready_len] = bid as u32;
                            self.ready_len += 1;
                        }
                        None => {
                            // 切り詰め: データグラム破棄。バッファは即返却対象に積む。
                            self.pending_return.push(bid);
                        }
                    }
                }
                None => {
                    if finished {
                        self.user_data = 0;
                    }
                    break;
                }
            }
        }

        if self.ready_len > 0 {
            return Ok(self.ready_len);
        }
        if let Some(e) = self.pending_error.take() {
            return Err(e);
        }
        Ok(0)
    }

    /// 1 件以上のデータグラムが完了するまで待つ Future。
    pub fn recv_batch(&mut self) -> MsRecvBatch<'_> {
        MsRecvBatch { inner: self }
    }

    /// `ready` の i 番目の bid を返す（0 <= i < 直近 `recv_batch()` の戻り値）。
    #[inline]
    pub fn ready_slot(&self, i: usize) -> usize {
        self.ready[i] as usize
    }

    /// 指定 bid の完了結果を取り出す（1 回のみ消費可能）。取り出した時点でそのバッファは
    /// 次の `rearm_ready()` でリングへ返却される（呼び出し側は返却前に処理を終えること）。
    pub fn take_result(&mut self, idx: usize) -> io::Result<SlotMeta> {
        let meta = self.results[idx]
            .take()
            .expect("take_result called on bid without a pending result");
        self.pending_return.push(idx as u16);
        Ok(meta)
    }

    /// 指定 bid のペイロード（可変。quiche recv / Header::from_slice 用）。
    pub fn payload_mut(&mut self, idx: usize, len: usize) -> &mut [u8] {
        let bid = idx as u16;
        let ptr = unsafe { self.buf_addr_ptr(bid).add(BUF_HDR + NAME_CAP + CMSG_CAP) };
        let len = len.min(PAYLOAD_CAP);
        // SAFETY: ptr は bid の provided buffer 内の payload 領域先頭で、PAYLOAD_CAP バイトの
        // 有効な書き込み可能領域を指す（バッファ確保時に固定）。len <= PAYLOAD_CAP。
        unsafe { std::slice::from_raw_parts_mut(ptr, len) }
    }

    /// 消費済みバッファをリングへ返却し（syscall なし）、multishot が終了していれば
    /// 新しい RECVMSG を 1 本アームして提出する。
    ///
    /// 呼び出し側は `ready` の全件を `take_result` で処理し終えた後に呼ぶこと（C1 と同じ契約）。
    pub fn rearm_ready(&mut self) -> io::Result<()> {
        self.republish_pending();
        self.ready_len = 0;
        if self.user_data == 0 {
            self.arm()?;
            submit_sqes()?;
        }
        Ok(())
    }
}

impl Drop for MultishotUdpRecv {
    fn drop(&mut self) {
        if self.user_data != 0 {
            // 未完了の multishot op が残っている: カーネルがまだ provided buffer 領域
            // （ring_ptr/buf_ptr）を参照している可能性があるため、即座に mmap 解放しては
            // ならない（use-after-free 防止）。detach_op に渡す OpGuard::Cleanup で
            // 「最終 CQE / キャンセル完了後」にのみ unregister + munmap を行う
            // （`PipelinedUdpRecv` と異なりバッファがリング経由でカーネルに常時公開されて
            // いるため、Noop では済ませられない）。
            let bgid = self.bgid;
            let ring_ptr = self.ring_ptr;
            let ring_size = self.ring_size;
            let buf_ptr = self.buf_ptr;
            let buf_region_size = self.buf_region_size;
            detach_op(
                self.user_data,
                OpGuard::Cleanup(Box::new(move |_res| {
                    // SAFETY: この時点でカーネルは当該 op（provided buffer を参照する
                    // multishot RECVMSG）の完了/キャンセルを終えており、以後
                    // ring_ptr/buf_ptr を参照しない。unregister はベストエフォート
                    // （リング自体が後続で破棄される場合は失敗しても解放を続行する）。
                    let _ = with_ring(|ring| ring.unregister_buf_ring(bgid));
                    unsafe {
                        libc::munmap(ring_ptr as *mut libc::c_void, ring_size);
                        libc::munmap(buf_ptr as *mut libc::c_void, buf_region_size);
                    }
                })),
            );
            self.user_data = 0;
        } else {
            // in-flight op なし: 直ちに解放してよい。
            let _ = with_ring(|ring| ring.unregister_buf_ring(self.bgid));
            unsafe {
                libc::munmap(self.ring_ptr as *mut libc::c_void, self.ring_size);
                libc::munmap(self.buf_ptr as *mut libc::c_void, self.buf_region_size);
            }
        }
    }
}

/// `MultishotUdpRecv::recv_batch` Future。
pub struct MsRecvBatch<'a> {
    inner: &'a mut MultishotUdpRecv,
}

impl Future for MsRecvBatch<'_> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self.get_mut().inner;

        match this.scan() {
            Ok(0) => {}
            Ok(n) => return Poll::Ready(Ok(n)),
            Err(e) => return Poll::Ready(Err(e)),
        }

        if this.user_data != 0 {
            set_op_waker(this.user_data, cx.waker().clone());
            return Poll::Pending;
        }
        // 呼び出し側の不変条件違反（rearm し忘れ）。C1 と同様、無限 Pending にせずエラーで
        // 検出できるようにする。
        Poll::Ready(Err(io::Error::other(
            "MultishotUdpRecv: no op in flight and none ready (missing rearm_ready?)",
        )))
    }
}

/// F-130 C1/C2 の受信バックエンド選択。
///
/// 呼び出し側（`http3_server.rs`）は C1（`PipelinedUdpRecv`）/C2（`MultishotUdpRecv`）の
/// どちらが選ばれても同じシグネチャ（`recv_batch`/`ready_slot`/`take_result`/
/// `payload_mut`/`rearm_ready`/`batch_size`）で扱える。
pub enum UdpRecvBackend {
    Multishot(MultishotUdpRecv),
    Pipelined(PipelinedUdpRecv),
}

impl UdpRecvBackend {
    /// **C2（真の multishot + buffer ring）は既定で無効（オプトイン）**であり、
    /// 環境変数 `VEIL_H3_BUFRING=1`（`true`/`on` も可）を指定したときだけ試みる。
    ///
    /// 既定を C1（実績のあるパイプライン化 RECVMSG）に据え置く理由は
    /// **実機で C2 を 1 度も動作させられていない**ためである: 開発・検証機の
    /// Linux 6.8.0-137-generic は `IORING_REGISTER_PBUF_RING` を（クリーンなリング・
    /// liburing 経由・`IOU_PBUF_RING_MMAP` 有無を問わず）`-EINVAL` で拒否する。
    /// したがって multishot の受信ハッピーパス（複数 CQE の drain・バッファ返却・
    /// GRO/切り詰め処理）は純関数の単体テストでしか検証できておらず、既定で
    /// 有効にすると「検証していない経路が本番で動く」ことになる。計測（交互 A/B）で
    /// 優位性を確認できるまで既定は C1 とする。
    ///
    /// オプトインした場合でも、登録・アームに失敗した環境では自動的に C1 へ
    /// フォールバックする（機能低下は起きない）。
    pub fn new(fd: RawFd, batch: usize) -> io::Result<Self> {
        let enable_bufring = std::env::var_os("VEIL_H3_BUFRING")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("on"))
            .unwrap_or(false);

        if enable_bufring {
            match MultishotUdpRecv::new(fd, batch) {
                Ok(m) => return Ok(UdpRecvBackend::Multishot(m)),
                Err(e) => {
                    ftlog::info!(
                        "[HTTP/3] multishot buffer-ring RECVMSG unavailable ({}), falling back to pipelined RECVMSG (C1)",
                        e
                    );
                }
            }
        }

        PipelinedUdpRecv::new(fd, batch).map(UdpRecvBackend::Pipelined)
    }

    #[inline]
    pub fn is_multishot(&self) -> bool {
        matches!(self, UdpRecvBackend::Multishot(_))
    }

    #[inline]
    pub fn batch_size(&self) -> usize {
        match self {
            UdpRecvBackend::Multishot(m) => m.batch_size(),
            UdpRecvBackend::Pipelined(p) => p.batch_size(),
        }
    }

    pub fn recv_batch(&mut self) -> RecvBatchAny<'_> {
        match self {
            UdpRecvBackend::Multishot(m) => RecvBatchAny::Multishot(m.recv_batch()),
            UdpRecvBackend::Pipelined(p) => RecvBatchAny::Pipelined(p.recv_batch()),
        }
    }

    #[inline]
    pub fn ready_slot(&self, i: usize) -> usize {
        match self {
            UdpRecvBackend::Multishot(m) => m.ready_slot(i),
            UdpRecvBackend::Pipelined(p) => p.ready_slot(i),
        }
    }

    pub fn take_result(&mut self, idx: usize) -> io::Result<SlotMeta> {
        match self {
            UdpRecvBackend::Multishot(m) => m.take_result(idx),
            UdpRecvBackend::Pipelined(p) => p.take_result(idx),
        }
    }

    pub fn payload_mut(&mut self, idx: usize, len: usize) -> &mut [u8] {
        match self {
            UdpRecvBackend::Multishot(m) => m.payload_mut(idx, len),
            UdpRecvBackend::Pipelined(p) => p.payload_mut(idx, len),
        }
    }

    pub fn rearm_ready(&mut self) -> io::Result<()> {
        match self {
            UdpRecvBackend::Multishot(m) => m.rearm_ready(),
            UdpRecvBackend::Pipelined(p) => p.rearm_ready(),
        }
    }

    /// C2 実行中に `-EINVAL`（buffer ring 登録には対応するが true multishot recv 自体は
    /// 未対応というカーネルギャップ、5.19〜6.0）を観測した場合、同じ fd で C1 へ 1 度だけ
    /// 切り替える。C2 でなければ何もしない。
    pub fn downgrade_to_pipelined_if_einval(&mut self, batch: usize) -> io::Result<bool> {
        let needs_downgrade = matches!(self, UdpRecvBackend::Multishot(m) if m.einval);
        if !needs_downgrade {
            return Ok(false);
        }
        if let UdpRecvBackend::Multishot(m) = self {
            let fd = m.fd();
            ftlog::info!(
                "[HTTP/3] multishot RECVMSG rejected at runtime (EINVAL); falling back to pipelined RECVMSG (C1)"
            );
            *self = UdpRecvBackend::Pipelined(PipelinedUdpRecv::new(fd, batch)?);
        }
        Ok(true)
    }
}

/// `UdpRecvBackend::recv_batch` Future（バックエンドごとの Future をまとめる）。
pub enum RecvBatchAny<'a> {
    Multishot(MsRecvBatch<'a>),
    Pipelined(RecvBatch<'a>),
}

impl Future for RecvBatchAny<'_> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY 不要: 両バリアントとも `&mut` 参照のみを保持する構造体で自己参照を持たず
        // Unpin なので、Pin 射影なしで安全に取り出せる。
        match self.get_mut() {
            RecvBatchAny::Multishot(f) => Pin::new(f).poll(cx),
            RecvBatchAny::Pipelined(f) => Pin::new(f).poll(cx),
        }
    }
}

#[cfg(test)]
mod c2_tests {
    use super::*;

    /// 正常系: recvmsg_out ヘッダ + AF_INET name + 空 control + payload を合成し、
    /// アドレス・ペイロード長が正しく切り出されることを検証する（純関数テスト）。
    #[test]
    fn parse_recvmsg_buf_extracts_header_name_and_payload() {
        let mut buf = vec![0u8; BUF_SIZE];

        let hdr = IoUringRecvmsgOut {
            namelen: NAME_CAP as u32,
            controllen: 0,
            payloadlen: 5,
            flags: 0,
        };
        // SAFETY: buf は BUF_SIZE(>= size_of::<IoUringRecvmsgOut>()) バイトのローカル Vec。
        unsafe {
            std::ptr::write_unaligned(buf.as_mut_ptr() as *mut IoUringRecvmsgOut, hdr);
        }

        let mut sin: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        sin.sin_family = libc::AF_INET as libc::sa_family_t;
        sin.sin_port = 4433u16.to_be();
        sin.sin_addr.s_addr = u32::from_ne_bytes([127, 0, 0, 1]);
        // SAFETY: name 領域は BUF_HDR..BUF_HDR+NAME_CAP で、sockaddr_in はそれより小さい。
        unsafe {
            std::ptr::copy_nonoverlapping(
                &sin as *const libc::sockaddr_in as *const u8,
                buf.as_mut_ptr().add(BUF_HDR),
                std::mem::size_of::<libc::sockaddr_in>(),
            );
        }

        let payload_off = BUF_HDR + NAME_CAP + CMSG_CAP;
        buf[payload_off..payload_off + 5].copy_from_slice(b"hello");

        let meta = parse_recvmsg_buf(&buf).expect("valid buffer must parse");
        assert_eq!(meta.payload_len, 5);
        assert_eq!(meta.from, "127.0.0.1:4433".parse().unwrap());
        assert!(meta.gro_segment_size.is_none());
    }

    /// namelen が要求容量（NAME_CAP）を超えている場合はデータグラムを破棄する（`None`）。
    #[test]
    fn parse_recvmsg_buf_discards_truncated_name() {
        let mut buf = vec![0u8; BUF_SIZE];
        let hdr = IoUringRecvmsgOut {
            namelen: (NAME_CAP + 1) as u32,
            controllen: 0,
            payloadlen: 0,
            flags: 0,
        };
        unsafe {
            std::ptr::write_unaligned(buf.as_mut_ptr() as *mut IoUringRecvmsgOut, hdr);
        }
        assert!(parse_recvmsg_buf(&buf).is_none());
    }

    /// controllen が要求容量（CMSG_CAP）を超えている場合も同様に破棄する。
    #[test]
    fn parse_recvmsg_buf_discards_truncated_control() {
        let mut buf = vec![0u8; BUF_SIZE];
        let hdr = IoUringRecvmsgOut {
            namelen: 0,
            controllen: (CMSG_CAP + 1) as u32,
            payloadlen: 0,
            flags: 0,
        };
        unsafe {
            std::ptr::write_unaligned(buf.as_mut_ptr() as *mut IoUringRecvmsgOut, hdr);
        }
        assert!(parse_recvmsg_buf(&buf).is_none());
    }

    /// `IoUringRecvmsgOut` のレイアウトが `namelen, controllen, payloadlen, flags`
    /// （オフセット 0/4/8/12、全 u32）であることをフィールド単位で確認する。
    #[test]
    fn recvmsg_out_field_offsets() {
        let hdr = IoUringRecvmsgOut {
            namelen: 1,
            controllen: 2,
            payloadlen: 3,
            flags: 4,
        };
        let bytes: [u8; 16] = unsafe { std::mem::transmute(hdr) };
        assert_eq!(u32::from_ne_bytes(bytes[0..4].try_into().unwrap()), 1);
        assert_eq!(u32::from_ne_bytes(bytes[4..8].try_into().unwrap()), 2);
        assert_eq!(u32::from_ne_bytes(bytes[8..12].try_into().unwrap()), 3);
        assert_eq!(u32::from_ne_bytes(bytes[12..16].try_into().unwrap()), 4);
    }
}
