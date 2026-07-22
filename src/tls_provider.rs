//! rustls 暗号プロバイダの選択（F-122）
//!
//! rustls は暗号バックエンドを「プロバイダ」として差し替えられる（`aws_lc_rs` /
//! `ring` の 2 択）。プラットフォームごとに最適なプロバイダを選ぶ:
//!
//! - **Linux / FreeBSD**: `aws_lc_rs`（AWS-LC）。kTLS（`ktls_rustls`）や HTTP/3 の
//!   quiche と AWS-LC ビルドを共有でき、アセンブリ最適化も効く。
//! - **OpenBSD**: `ring`。OpenBSD では aws-lc-rs が TLS ハンドシェイクの暗号処理を
//!   完了できない（ClientHello 受信後 ServerHello を生成せずスタックする。Rust Tier 3 +
//!   AWS-LC の OpenBSD サポート不足。F-122 で ktrace により確定）。`ring` は OpenBSD で
//!   実績があり、HTTPS 終端に必要な TLS1.2/1.3 の AEAD スイートを提供する。
//! - **macOS**: `ring`（F-125）。Docker（cargo-zigbuild）での universal2 クロスビルドでは
//!   aws-lc-sys の手書きアセンブリ `.S.o` を zig リンカが解釈できずリンク失敗し
//!   （`unknown cpu architecture`）、`AWS_LC_SYS_NO_ASM` も release ビルドでは禁止される。
//!   `ring` は cargo-zigbuild での apple-darwin クロスビルド実績があり、これを採用する。
//!   macOS は kTLS・http3 とも非対応のため AWS-LC 共有の利点も無く、ring で完結する。
//! - **Windows**（v0.6.0）: `ring`。aws-lc-sys のビルドは cmake + NASM を要求し、
//!   `messense/cargo-xwin` コンテナには NASM が無いため `x86_64-pc-windows-msvc`
//!   クロスビルドが通らない。Windows も kTLS・http3 非対応のため AWS-LC 共有の利点が
//!   無く、macOS/OpenBSD と同じ ring 経路に合流させる。
//!
//! rustls の `aws_lc_rs` / `ring` は同一の公開 API（`default_provider()` /
//! `ALL_CIPHER_SUITES` / `cipher_suite`）を持つモジュールのため、`pub use ... as`
//! の別名再エクスポートで呼び出し側を単一化する。Cargo 側では target 別依存で
//! 非 OpenBSD/macOS/Windows は `aws_lc_rs` のみ・OpenBSD/macOS/Windows は `ring` のみを
//! リンクする（Linux ビルドは不変）。
//!
//! kTLS 経路（`src/ktls.rs` / `src/ktls_rustls.rs` / `src/ktls_freebsd.rs`）は
//! `veil_ktls`（Linux/FreeBSD 限定、F-126）でのみコンパイルされ AWS-LC 固有 API
//! （cipher_suite 定数）に依存するため、本モジュールでは抽象化せず各所で直接
//! `aws_lc_rs` を参照する（OpenBSD/macOS では非コンパイル）。

/// このプラットフォームで使う rustls 暗号プロバイダモジュール。
///
/// `provider::default_provider()` / `provider::ALL_CIPHER_SUITES` の形で参照する。
// aarch64-windows は aws_lc_rs（ARM asm・NASM 不要、cmake でクロスビルド可。ring 0.17 は
// aarch64-pc-windows-msvc の prebuilt asm を持たず cargo-xwin でビルド不能）。
// x86_64-windows/macOS/OpenBSD は ring。Cargo.toml の provider target 分割と一致させること。
#[cfg(not(target_os = "openbsd"))]
pub use rustls::crypto::aws_lc_rs as provider;
#[cfg(target_os = "openbsd")]
pub use rustls::crypto::ring as provider;

/// HTTP/3（quiche）の乱数生成に使う `SecureRandom` 実装。
///
/// 非 OpenBSD は aws-lc-rs、OpenBSD は ring の `SystemRandom` を用いる
/// （どちらも `rustls`/`quiche` とは独立した RNG API）。http3 feature 有効時のみ使用。
#[cfg(all(feature = "http3", not(target_os = "openbsd")))]
pub use aws_lc_rs::rand::{SecureRandom, SystemRandom};
#[cfg(all(feature = "http3", target_os = "openbsd"))]
pub use ring::rand::{SecureRandom, SystemRandom};
