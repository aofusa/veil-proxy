//! 非同期 TcpListener / TcpStream（reactor バックエンド）のプラットフォーム分岐
//!
//! Unix（Linux `--features epoll` / FreeBSD / OpenBSD / macOS）は Berkeley ソケット API
//! （`libc`）で実装する（[`unix`] モジュール）。Windows は Winsock（`windows-sys`）で
//! 実装する（[`windows`] モジュール、F-125/v0.6.0）。公開 API（型・メソッドシグネチャ）は
//! 両モジュールで同一に保ち、呼び出し側（`proxy.rs` 等）は cfg 分岐を意識しない。

#[cfg(not(windows))]
mod unix;
#[cfg(not(windows))]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;
