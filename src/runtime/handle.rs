//! クロスプラットフォームなソケット/FD ハンドル型（F-125 Windows 対応、v0.6.0）
//!
//! Unix は `RawFd`（i32）をそのまま使う。Windows には `RawFd` が無く、代わりに
//! Winsock の `SOCKET`（64-bit 環境では `UINT_PTR` = `u64`）を使う。
//!
//! reactor 層（`runtime::reactor::tcp`）は既存コードで「`-1` = 未確保」という
//! センチネル値を多用する（`Connect::fd` 等）。`SOCKET` は符号なしのため `-1` を
//! そのまま持てないが、Winsock の `INVALID_SOCKET` は `!0u64`（全ビット 1）と
//! 定義されており、これを `isize` として再解釈するとビットパターンが `-1isize` と
//! 一致する。そこで本モジュールは Windows 側のハンドル型を `isize` として定義し、
//! FFI 境界（socket 生成・close 等）でのみ `SOCKET`（`u64`）との相互変換を行う。
//! これにより reactor 層のロジック（`-1` センチネル・比較演算）を Unix 版から
//! ほぼ変更せずに再利用できる。
//!
//! `AsRawFd` は Unix 標準ライブラリの trait をそのまま re-export し、Windows では
//! 同名・同シグネチャの crate ローカル trait を用意する（呼び出し側は
//! `use crate::runtime::handle::{AsRawFd, RawFd};` のみで両対応する）。
//!
//! 本モジュール（および `RawFd`/`AsRawFd`）は `pub`（`pub(crate)` ではない）にしてある。
//! `Http2Connection<S>`（`pub struct`）のような公開型が
//! `impl<S: AsRawFd> Http2Connection<S>` の形で本モジュールの型/トレイトを境界に使うため、
//! `pub(crate)` のままだと「公開アイテムがより非公開なトレイト/型を露出している」という
//! `private_bounds`/`private_interfaces` 警告になる（Unix で `std::os::unix::io::AsRawFd`
//! を直に使えば発生しないが、両 platform で同じ `crate::runtime::handle::*` を使う設計上、
//! モジュール自体を `pub` にして揃えている）。

#[cfg(unix)]
pub use std::os::unix::io::RawFd;

/// Windows 版ハンドル型。`SOCKET`（`u64`）を `isize` として再解釈したもの。
/// `INVALID_SOCKET`（`!0u64`）は `-1isize` のビットパターンと一致するため、
/// 既存の `-1` センチネル比較がそのまま使える。
#[cfg(windows)]
pub type RawFd = isize;

/// `RawFd` を保持しない状態を表す値（Windows: `INVALID_SOCKET` 相当のビットパターン）。
/// 現状 Windows の `reactor/tcp/windows.rs` でのみセンチネルとして使用するため
/// `#[cfg(windows)]` に限定する（Unix 経路では未使用のため dead_code を避ける）。
#[cfg(windows)]
pub(crate) const INVALID_FD: RawFd = -1;

#[cfg(unix)]
pub use std::os::unix::io::AsRawFd;

/// Windows 版 `AsRawFd` 相当 trait（crate ローカル）。
#[cfg(windows)]
pub trait AsRawFd {
    fn as_raw_fd(&self) -> RawFd;
}

/// `RawFd`（`isize`）と Winsock `SOCKET`（`u64`）の相互変換ヘルパー（Windows 専用）。
#[cfg(windows)]
pub(crate) mod win {
    use super::RawFd;
    use windows_sys::Win32::Networking::WinSock::SOCKET;

    #[inline]
    pub(crate) fn to_socket(fd: RawFd) -> SOCKET {
        fd as usize as SOCKET
    }

    #[inline]
    pub(crate) fn from_socket(sock: SOCKET) -> RawFd {
        sock as isize
    }
}
