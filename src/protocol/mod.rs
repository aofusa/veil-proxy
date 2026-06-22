//! # プロトコル抽象化モジュール
//!
//! HTTP/1.1, HTTP/2, HTTP/3 のプロトコルネゴシエーションと
//! 共通インターフェースを提供します。

pub mod negotiation;

pub use negotiation::{
    configure_alpn_h2, configure_alpn_h2_client, get_negotiated_protocol, HttpProtocol,
};
