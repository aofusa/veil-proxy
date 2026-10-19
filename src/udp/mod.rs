//! # UDP ソケット (HTTP/3 用)
//!
//! 独自ランタイム（io_uring / readiness reactor）と統合した高パフォーマンス UDP ソケットを提供します。
//! GSO (Generic Segmentation Offload) と GRO (Generic Receive Offload) をサポート。

pub mod socket;

pub use socket::QuicUdpSocket;
