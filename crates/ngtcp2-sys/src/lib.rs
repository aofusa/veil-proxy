//! ngtcp2 の低レベル FFI バインディング。
//!
//! C ライブラリは build.rs で公式リポジトリ
//! <https://github.com/ngtcp2/ngtcp2> から静的ビルドする。
//! TLS は **aws-lc-sys**（BoringSSL 互換）と `ngtcp2_crypto_boringssl` で統合する。

#![allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code,
    clippy::all
)]

include!("bindings.rs");
