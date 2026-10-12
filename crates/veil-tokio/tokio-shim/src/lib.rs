//! tokio の API を veil のランタイムで提供する置き換え用クレート（F-198）。中身は `veil::tokio` の再エクスポート。
#![allow(clippy::all)]

pub use veil::tokio::*;
