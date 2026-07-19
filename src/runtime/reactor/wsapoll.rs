//! `WSAPoll` の薄いラッパ（`veil_poller_wsapoll`、Windows）
//!
//! epoll/kqueue と異なり `WSAPoll` は **レベルトリガのみ**で oneshot 相当の登録モードを
//! 持たない。カーネル側に「関心を持つ fd の集合」を事前登録する API も無く、呼び出しの
//! たびに監視したい `WSAPOLLFD` 配列を丸ごと渡す設計になっている。そのため本実装は
//! epoll/kqueue のような「登録 → カーネルが覚えている」方式ではなく、`park()` が呼ばれる
//! たびに [`super::poller::FdTable`] から **現在 armed（待機中）な fd** を毎回列挙して
//! `WSAPOLLFD` 配列を組み立て、`WSAPoll` へ渡す。oneshot 相当の意味論は「発火した方向の
//! armed ビットを落とす（`dispatch_event`）」ことで表現する（次に読み書きしたい側が
//! 改めて `register_read`/`register_write` で armed を立て直す）。
//!
//! ソケットハンドルは [`crate::runtime::handle::RawFd`]（`isize` に再解釈した `SOCKET`）を
//! そのまま使う。

use std::io;

use crate::runtime::handle::{win, RawFd};
use windows_sys::Win32::Networking::WinSock::{
    WSAPoll, POLLERR, POLLHUP, POLLRDNORM, POLLWRNORM, WSAPOLLFD,
};

/// 読み取り可能 interest ビット（`POLLRDNORM` 相当）。
pub const READ: u32 = 0b01;
/// 書き込み可能 interest ビット（`POLLWRNORM` 相当）。
pub const WRITE: u32 = 0b10;
/// エラー/ハングアップ通知ビット（要求せずとも常に配送される）。
pub const ERR_HUP: u32 = 0b100;

/// `WSAPoll` バックエンドのポーラー。
///
/// epoll/kqueue と異なり保持すべきカーネルインスタンス（fd）が無いため、実質的には
/// マーカー型（Unit 構造体）。`fd_table` から都度配列を組み立てて `WSAPoll` を呼ぶ。
pub(crate) struct WsaPollPoller {
    _private: (),
}

impl WsaPollPoller {
    pub fn new() -> io::Result<Self> {
        Ok(Self { _private: () })
    }

    /// `entries`（`(fd, armed)` の一覧）を元に `WSAPoll` を実行し、発火した
    /// `(fd, revents ビット)` を `out` へ積む。`timeout_ms` は負値で無期限待機。
    pub fn wait(
        &self,
        entries: &[(RawFd, u32)],
        out: &mut Vec<(RawFd, u32)>,
        timeout_ms: i32,
    ) -> io::Result<usize> {
        out.clear();
        if entries.is_empty() {
            // WSAPoll は 0 件配列だとすぐ返る実装依存の挙動があるため、タイマーのみの
            // 待機はこの関数を呼ばず executor 側で sleep するべきだが、防御的に
            // タイムアウト分だけ素朴に待つ（fd 監視なしの block_on ループ用）。
            if timeout_ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(timeout_ms as u64));
            }
            return Ok(0);
        }

        let mut fds: Vec<WSAPOLLFD> = entries
            .iter()
            .map(|(fd, armed)| {
                let mut events: i16 = 0;
                if armed & READ != 0 {
                    events |= POLLRDNORM as i16;
                }
                if armed & WRITE != 0 {
                    events |= POLLWRNORM as i16;
                }
                WSAPOLLFD {
                    fd: win::to_socket(*fd),
                    events,
                    revents: 0,
                }
            })
            .collect();

        let n = unsafe { WSAPoll(fds.as_mut_ptr(), fds.len() as u32, timeout_ms) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            return Ok(0);
        }

        for (i, pfd) in fds.iter().enumerate() {
            if pfd.revents == 0 {
                continue;
            }
            let mut bits = 0u32;
            if pfd.revents & (POLLRDNORM as i16) != 0 {
                bits |= READ;
            }
            if pfd.revents & (POLLWRNORM as i16) != 0 {
                bits |= WRITE;
            }
            if pfd.revents & ((POLLERR | POLLHUP) as i16) != 0 {
                bits |= ERR_HUP;
            }
            if bits != 0 {
                out.push((entries[i].0, bits));
            }
        }
        Ok(out.len())
    }
}
