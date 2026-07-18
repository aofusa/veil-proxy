//! nghttp3 の低レベル FFI バインディング。
//!
//! C ライブラリは build.rs で公式リポジトリから静的ビルドする。

#![allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code,
    clippy::all
)]

include!("bindings.rs");
