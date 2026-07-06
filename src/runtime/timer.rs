//! タイムアウト管理（io_uring TIMEOUT op）
//!
//! io_uring の `IORING_OP_TIMEOUT` を使用したタイムアウト Future を実装する。
//! monoio の `monoio::time::timeout` / `monoio::time::sleep` に相当する。

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use crate::runtime::executor::{
    alloc_op, detach_op_no_cancel, peek_op_result, remove_op, set_op_waker, submit_sqes,
    take_op_result, with_ring, OpGuard,
};
use crate::runtime::ring::{KernelTimespec, IORING_OP_TIMEOUT};

// ====================
// Sleep Future
// ====================

/// タイムアウト/スリープ Future
///
/// `IORING_OP_TIMEOUT` を使って io_uring でタイムアウトを管理する。
pub struct Sleep {
    /// user_data（操作識別子）
    user_data: u64,
    /// タイムアウト時刻（カーネル timespec）
    ts: KernelTimespec,
    /// SQE 提出済みフラグ
    submitted: bool,
}

impl Sleep {
    /// 指定した Duration 後に完了する Sleep Future を作成する
    pub fn new(duration: Duration) -> Self {
        let ts = KernelTimespec {
            tv_sec: duration.as_secs() as i64,
            tv_nsec: duration.subsec_nanos() as i64,
        };
        Self {
            // F-46: スロットは初回 poll（SQE 提出時）に確保する。0 は無効 ID で、
            // 未提出のまま peek/take されてもどのスロットとも一致しない。
            user_data: 0,
            ts,
            submitted: false,
        }
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let user_data = self.user_data;

        // 完了チェック
        if let Some(res) = peek_op_result(user_data) {
            take_op_result(user_data);
            // ETIME (-62) はタイムアウト正常完了
            // ECANCELED (-125) はキャンセル
            let _ = res;
            return Poll::Ready(());
        }

        if !self.submitted {
            // SQE を提出（op スロットを確保）
            let user_data = alloc_op();
            self.user_data = user_data;

            let ts_ptr = &self.ts as *const KernelTimespec as u64;

            let acquired = with_ring(|ring| {
                if let Some(sqe) = ring.get_sqe_or_submit() {
                    sqe.opcode = IORING_OP_TIMEOUT;
                    sqe.fd = -1;
                    sqe.addr_or_splice_off_in = ts_ptr;
                    sqe.len = 1; // 件数
                    sqe.user_data = user_data;
                    sqe.off_or_addr2 = 0; // 絶対タイムアウトカウント 0 = 相対タイムアウト
                    true
                } else {
                    false
                }
            });
            if !acquired {
                // B-24: SQ/CQ 枯渇で TIMEOUT SQE を確保できず。op を解放し、満了扱いで完了する
                //（submitted を立てず永久ハングを避ける。`timeout()` は内側 Future を畳む）。
                remove_op(user_data);
                return Poll::Ready(());
            }

            if let Err(e) = submit_sqes() {
                remove_op(user_data);
                ftlog::error!("Sleep submit failed: {}", e);
                return Poll::Ready(());
            }

            self.submitted = true;
        }

        // Waker を登録（初回 poll では上でスロット確保済みの self.user_data を使う）
        set_op_waker(self.user_data, cx.waker().clone());
        Poll::Pending
    }
}

// Sleep に FusedFuture を実装（futures::select_biased! で使用するため）
impl futures::future::FusedFuture for Sleep {
    fn is_terminated(&self) -> bool {
        if !self.submitted {
            return false;
        }
        crate::runtime::executor::peek_op_result(self.user_data).is_some()
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        if self.submitted && take_op_result(self.user_data).is_none() {
            // in-flight のまま drop された（`timeout()` で内側 Future が勝った場合等）。
            // 旧実装はエントリを残したまま放置しテーブルをリークしていた（F-46 で修正）。
            // TIMEOUT はカーネル参照バッファを持たず自然完了するため、キャンセルは投げず
            // Noop ガードで detach し、満了 CQE の到着時にスロットを解放する。
            detach_op_no_cancel(self.user_data, OpGuard::Noop);
        }
    }
}

// ====================
// sleep / timeout API
// ====================

/// 指定した Duration スリープする
pub fn sleep(duration: Duration) -> Sleep {
    Sleep::new(duration)
}

/// Future にタイムアウトを設定する
///
/// タイムアウト前に Future が完了すれば `Ok(R)` を返す。
/// タイムアウトした場合は `Err(Elapsed)` を返す。
pub async fn timeout<F, R>(duration: Duration, future: F) -> Result<R, Elapsed>
where
    F: Future<Output = R>,
{
    futures::select_biased! {
        result = futures::FutureExt::fuse(future) => Ok(result),
        _ = futures::FutureExt::fuse(sleep(duration)) => Err(Elapsed),
    }
}

/// タイムアウトエラー
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed;

impl std::fmt::Display for Elapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "deadline has elapsed")
    }
}

impl std::error::Error for Elapsed {}
