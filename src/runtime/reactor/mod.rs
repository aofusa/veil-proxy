//! epoll ベース readiness ランタイム（F-120 Phase 2、`veil_rt_reactor`）
//!
//! io_uring 完了ベースの `uring` バックエンドに対し、readiness（準備完了通知）
//! ベースで動作するバックエンド。poller の実体は cfg で選択する（trait object は
//! 使わずコンパイル時に 1 実装へ確定させる）:
//!
//! - `veil_poller_epoll`（Linux + `--features epoll`）: [`epoll`] モジュール
//! - `veil_poller_kqueue`（FreeBSD/OpenBSD）: [`kqueue`] モジュール（F-120 Phase 4）
//!
//! ## 設計要点
//!
//! - 各 I/O は「非ブロッキング syscall を先に試す → `WouldBlock` なら oneshot readiness を
//!   登録して `Pending` を返す → 起床後に再試行する」という **try-first** パターンで実装する。
//! - io_uring と異なり、readiness モデルでは「カーネルが Future 保有のバッファを非同期に
//!   参照し続ける」ことが構造的に発生しない（実際の read/write syscall は Future が
//!   起床して再 poll された時点で同期的に発行する）。そのため io_uring 版の
//!   `OpGuard`/`detach_op`（B-07 UAF 対策）に相当する後始末機構は不要であり、各 Future の
//!   `Drop` は「待機登録（Waker）を破棄する」以上のことをしなくてよい。
//! - fd ごとの読み/書き待機状態は [`poller::FdTable`] が fd 番号でインデックスして保持する。
//!   同一 fd に読み待ちと書き待ちが同時に存在できる（L4/splice の双方向転送で必要）。

#[cfg(veil_poller_epoll)]
pub(crate) mod epoll;

#[cfg(veil_poller_kqueue)]
pub(crate) mod kqueue;

#[cfg(veil_poller_wsapoll)]
pub(crate) mod wsapoll;

/// FreeBSD POSIX AIO 経路（F-127）。`--features aio` 指定時のみコンパイルされる。
#[cfg(veil_aio)]
pub(crate) mod aio;

pub(crate) mod poller;

pub mod executor;
/// splice(2) は Linux 専用（BSD は呼び出し側で read/write 転送へフォールバック。設計 3.3 節）。
#[cfg(target_os = "linux")]
pub mod splice;
pub mod tcp;
pub mod timer;

// `crate::runtime::mod` が `reactor::{executor, splice, tcp, timer}` をモジュール単位で
// re-export し、そこから個々のシンボル（`spawn`/`TcpStream`/`sleep` 等）を再度 re-export
// するため、本モジュールでの重複 re-export はしない。
