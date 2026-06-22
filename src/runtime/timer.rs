//! タイムアウト管理（io_uring TIMEOUT op）
//!
//! io_uring の `IORING_OP_TIMEOUT` を使用したタイムアウト Future を実装する。
//! monoio の `monoio::time::timeout` / `monoio::time::sleep` に相当する。

#![allow(dead_code)]

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use crate::runtime::executor::{
    next_user_data, peek_op_result, register_op, set_op_waker, submit_sqes, take_op_result,
    with_ring,
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
        let user_data = next_user_data();
        let ts = KernelTimespec {
            tv_sec: duration.as_secs() as i64,
            tv_nsec: duration.subsec_nanos() as i64,
        };
        Self {
            user_data,
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
            // SQE を提出
            register_op(user_data);

            let ts_ptr = &self.ts as *const KernelTimespec as u64;

            with_ring(|ring| {
                if let Some(sqe) = ring.get_sqe() {
                    sqe.opcode = IORING_OP_TIMEOUT;
                    sqe.fd = -1;
                    sqe.addr_or_splice_off_in = ts_ptr;
                    sqe.len = 1; // 件数
                    sqe.user_data = user_data;
                    sqe.off_or_addr2 = 0; // 絶対タイムアウトカウント 0 = 相対タイムアウト
                }
            });

            if let Err(e) = submit_sqes() {
                ftlog::error!("Sleep submit failed: {}", e);
                return Poll::Ready(());
            }

            self.submitted = true;
        }

        // Waker を登録
        set_op_waker(user_data, cx.waker().clone());
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
        // キャンセルは ASYNC_CANCEL で行うが、シンプル化のため省略
        // タイムアウトは自然に完了する
        if self.submitted {
            take_op_result(self.user_data);
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
