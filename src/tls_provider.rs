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
//!
//! - **`system-tls`**（F-137）: 上記のいずれとも独立に、`rustls-openssl`（システムの
//!   libcrypto = OpenSSL / LibreSSL を呼ぶ CryptoProvider 実装）へ切り替える。
//!   rustls / quiche 本体はそのまま使い、依存する暗号ライブラリだけを動的リンクへ
//!   差し替えるための feature であり、`veil_ktls`（AWS-LC 固有 API 前提）とは併用
//!   できない（`build.rs` が `ktls` + `system-tls` 同時指定時に `veil_ktls` cfg を
//!   立てず自動的に kTLS を無効化する）。

/// このプラットフォームで使う rustls 暗号プロバイダモジュール。
///
/// `provider::default_provider()` / `provider::ALL_CIPHER_SUITES` の形で参照する。
// aarch64-windows は aws_lc_rs（ARM asm・NASM 不要、cmake でクロスビルド可。ring 0.17 は
// aarch64-pc-windows-msvc の prebuilt asm を持たず cargo-xwin でビルド不能）。
// x86_64-windows/macOS/OpenBSD は ring。Cargo.toml の provider target 分割と一致させること。
// system-tls: システム libcrypto（OpenSSL / LibreSSL）を使う（F-137）。target_os に
// 関わらず最優先で選択する。
#[cfg(feature = "system-tls")]
pub use rustls_openssl as provider;

#[cfg(all(not(feature = "system-tls"), not(target_os = "openbsd")))]
pub use rustls::crypto::aws_lc_rs as provider;
#[cfg(all(not(feature = "system-tls"), target_os = "openbsd"))]
pub use rustls::crypto::ring as provider;

/// HTTP/3（quiche）の乱数生成に使う `SecureRandom` 実装。
///
/// 非 OpenBSD は aws-lc-rs、OpenBSD は ring の `SystemRandom` を用いる
/// （どちらも `rustls`/`quiche` とは独立した RNG API）。http3 feature 有効時のみ使用。
/// `system-tls` 時は `rustls-openssl` に同等の RNG API が無いため、
/// `openssl::rand::rand_bytes` を包む薄いシムを使う（呼び出し形は同じ）。
#[cfg(all(feature = "http3", feature = "system-tls"))]
pub use system_tls_rand::{SecureRandom, SystemRandom};
#[cfg(all(feature = "http3", not(feature = "system-tls"), not(target_os = "openbsd")))]
pub use aws_lc_rs::rand::{SecureRandom, SystemRandom};
#[cfg(all(feature = "http3", not(feature = "system-tls"), target_os = "openbsd"))]
pub use ring::rand::{SecureRandom, SystemRandom};

/// `system-tls` 時の HTTP/3（quiche）RNG シム（F-137）。
///
/// `aws_lc_rs::rand`/`ring::rand` の `SystemRandom::new()` + `fill(&mut buf) -> Result<(), _>`
/// と同じ呼び出し形を提供し、`src/http3_server.rs` を無変更に保つ。実体は
/// `openssl::rand::rand_bytes`（システム libcrypto の CSPRNG）を呼ぶだけの薄いラッパー。
#[cfg(all(feature = "http3", feature = "system-tls"))]
mod system_tls_rand {
    /// `fill` が失敗したことを示すだけの空エラー型（呼び出し側は理由を問わず
    /// `.map_err(|_| ...)` するため詳細情報は不要）。
    #[derive(Debug)]
    pub struct RandError;

    /// 乱数生成器のハンドル（状態を持たない。システムの CSPRNG を毎回呼ぶ）。
    pub struct SystemRandom;

    impl SystemRandom {
        pub fn new() -> Self {
            SystemRandom
        }
    }

    impl Default for SystemRandom {
        fn default() -> Self {
            Self::new()
        }
    }

    /// `aws_lc_rs::rand::SecureRandom` / `ring::rand::SecureRandom` と同じ形の trait。
    pub trait SecureRandom {
        fn fill(&self, dest: &mut [u8]) -> Result<(), RandError>;
    }

    impl SecureRandom for SystemRandom {
        fn fill(&self, dest: &mut [u8]) -> Result<(), RandError> {
            openssl::rand::rand_bytes(dest).map_err(|_| RandError)
        }
    }
}
