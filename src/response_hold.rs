//! HTTP/1.1 のパイプラインの応答を溜めて 1 回で送る（F-322）。
//!
//! クライアントが応答を待たずに続けて送った要求（パイプライン）では、応答を書く時点で次の要求が蓄積に届いている。
//! そのあいだは応答を送らずにここへ溜め、次の要求が届いていない要求の応答と一緒に 1 回の `send(2)`（TLS は暗号化と
//! 1 回の `writev(2)`）で送る。サーバ側の TLS ストリーム（`ktls_rustls::KtlsServerStream`・
//! `simple_tls::SimpleTlsServerStream`）が 1 つずつ持ち、ストリームの読み書きは溜めた分を先に送る（応答の順と、
//! 送らずに読みを待って止まらないことを保つ）。ソケットへ直接書く経路（`sendfile(2)`・`splice(2)`）は、その前に
//! 要求の処理（`proxy::h1`）が送る。

use std::cell::RefCell;

/// 溜めた量の上限（バッファの容量。足すと超える応答は溜めずに、溜めた分を送ってから書く）
pub(crate) const HOLD_MAX: usize = 16 * 1024;

/// スレッドのプールに置くバッファの数の上限
const POOL_CAP: usize = 64;

thread_local! {
    /// 溜めるバッファのプール（容量はどれも `HOLD_MAX`）。溜め始めた接続が借り、送ったら返す
    static POOL: RefCell<Vec<Vec<u8>>> = const { RefCell::new(Vec::new()) };
}

/// 溜めた応答と、今の要求の応答を溜めてよいか
#[derive(Default)]
pub(crate) struct ResponseHold {
    /// 溜めた応答（溜めていないときは容量 0）
    buf: Vec<u8>,
    /// 次の要求が蓄積に届いている（パイプライン）。今の要求の応答は溜めてよい
    next_ready: bool,
}

impl ResponseHold {
    /// 溜めた応答が無い
    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// 今の要求の後ろに次の要求が届いているか（要求ごとに要求の処理が設定する）
    #[inline]
    pub(crate) fn set_next_ready(&mut self, next_ready: bool) {
        self.next_ready = next_ready;
    }

    /// 今の要求の応答を溜めてよい
    #[inline]
    pub(crate) fn next_ready(&self) -> bool {
        self.next_ready
    }

    /// `n` バイトの応答を足しても上限に収まる
    #[inline]
    pub(crate) fn fits(&self, n: usize) -> bool {
        self.buf.len() + n <= HOLD_MAX
    }

    /// 応答を溜める。呼ぶ前に [`Self::fits`] で確かめる（バッファを伸ばさない）
    pub(crate) fn push(&mut self, data: &[u8]) {
        debug_assert!(self.fits(data.len()));
        if self.buf.capacity() == 0 {
            self.buf = POOL
                .with(|p| p.borrow_mut().pop())
                .unwrap_or_else(|| Vec::with_capacity(HOLD_MAX));
        }
        self.buf.extend_from_slice(data);
    }

    /// 溜めた分を取り出す（送ったら [`recycle`] へ返す）
    #[inline]
    pub(crate) fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buf)
    }
}

/// 送り終えたバッファをプールへ返す
pub(crate) fn recycle(mut buf: Vec<u8>) {
    if buf.capacity() != HOLD_MAX {
        return;
    }
    buf.clear();
    POOL.with(|p| {
        let mut p = p.borrow_mut();
        if p.len() < POOL_CAP {
            p.push(buf);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_until_the_limit_and_reuses_the_buffer() {
        let mut h = ResponseHold::default();
        assert!(h.is_empty() && !h.next_ready());
        h.set_next_ready(true);
        assert!(h.fits(HOLD_MAX));
        h.push(b"HTTP/1.1 200 OK\r\n\r\n");
        h.push(b"HTTP/1.1 204 No Content\r\n\r\n");
        assert!(!h.is_empty());
        assert!(!h.fits(HOLD_MAX));
        let b = h.take();
        assert!(h.is_empty());
        assert_eq!(
            &b[..],
            b"HTTP/1.1 200 OK\r\n\r\nHTTP/1.1 204 No Content\r\n\r\n"
        );
        let ptr = b.as_ptr();
        recycle(b);
        // 返したバッファを次に溜める接続が使う
        let mut h2 = ResponseHold::default();
        h2.push(b"x");
        let reused = h2.take();
        assert_eq!(reused.as_ptr(), ptr);
    }
}
