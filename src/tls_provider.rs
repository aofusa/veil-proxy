//! rustls 暗号プロバイダの選択（F-122）
//!
//! rustls は暗号バックエンドを「プロバイダ」として差し替えられる（`aws_lc_rs` /
//! `ring` の 2 択）。プラットフォームごとに最適なプロバイダを選ぶ:
//!
//! - **Linux / FreeBSD / macOS / Windows**: `aws_lc_rs`（AWS-LC）。Linux/FreeBSD では kTLS
//!   （`ktls_rustls`）や HTTP/3 の quiche と AWS-LC ビルドを共有でき、アセンブリ最適化も効く。
//!   macOS と Windows も F-131 で `ring` から `aws_lc_rs` へ統一した（当初 F-125 / v0.6.0 では
//!   cargo-zigbuild の `.S.o` リンク失敗と cargo-xwin コンテナの NASM 不足のため `ring` だった）。
//! - **OpenBSD**: `ring`。OpenBSD では aws-lc-rs が TLS ハンドシェイクの暗号処理を
//!   完了できない（ClientHello 受信後 ServerHello を生成せずスタックする。Rust Tier 3 +
//!   AWS-LC の OpenBSD サポート不足。F-122 で ktrace により確定）。`ring` は OpenBSD で
//!   実績があり、HTTPS 終端に必要な TLS1.2/1.3 の AEAD スイートを提供する。
//! - **NetBSD**（F-140）: OpenBSD と同じ `ring`（未検証だが aws-lc-rs の Tier 3 サポート
//!   不足という同じ懸念があるため、実証済みの OpenBSD 側に保守的に合わせる）。
//!
//! rustls の `aws_lc_rs` / `ring` は同一の公開 API（`default_provider()` /
//! `ALL_CIPHER_SUITES` / `cipher_suite`）を持つモジュールのため、`pub use ... as`
//! の別名再エクスポートで呼び出し側を単一化する。Cargo 側では target 別依存で
//! OpenBSD/NetBSD 以外は `aws_lc_rs` のみ・OpenBSD/NetBSD は `ring` のみをリンクする。
//!
//! kTLS 経路（`src/ktls.rs` / `src/ktls_rustls.rs` / `src/ktls_freebsd.rs`）は
//! `veil_ktls`（Linux/FreeBSD 限定、F-126）でのみコンパイルされ AWS-LC 固有 API
//! （cipher_suite 定数）に依存するため、本モジュールでは抽象化せず各所で直接
//! `aws_lc_rs` を参照する（OpenBSD/NetBSD/macOS/Windows では非コンパイル）。

/// このプラットフォームで使う rustls 暗号プロバイダモジュール。
///
/// `provider::default_provider()` / `provider::ALL_CIPHER_SUITES` の形で参照する。
// OpenBSD/NetBSD は ring、それ以外（Linux/FreeBSD/macOS/Windows x86_64・aarch64）は aws_lc_rs。
// Cargo.toml の provider target 分割と一致させること。
#[cfg(not(any(target_os = "openbsd", target_os = "netbsd")))]
pub use rustls::crypto::aws_lc_rs as provider;
#[cfg(any(target_os = "openbsd", target_os = "netbsd"))]
pub use rustls::crypto::ring as provider;

/// HTTP/3（quiche）の乱数生成に使う `SecureRandom` 実装。
///
/// 非 OpenBSD/NetBSD は aws-lc-rs、OpenBSD/NetBSD は ring の `SystemRandom` を用いる
/// （どちらも `rustls`/`quiche` とは独立した RNG API）。http3 feature 有効時のみ使用。
#[cfg(all(
    feature = "http3",
    not(any(target_os = "openbsd", target_os = "netbsd"))
))]
pub use aws_lc_rs::rand::{SecureRandom, SystemRandom};
#[cfg(all(feature = "http3", any(target_os = "openbsd", target_os = "netbsd")))]
pub use ring::rand::{SecureRandom, SystemRandom};

/// HTTP/3 のサーバ接続 ID 導出（クライアントの元 DCID → 決定的な SCID、B-94）に使う HMAC。
/// 乱数と同じくプラットフォームの暗号プロバイダに揃える（API は aws-lc-rs と ring で同一）。
#[cfg(all(
    feature = "http3",
    not(any(target_os = "openbsd", target_os = "netbsd"))
))]
pub use aws_lc_rs::hmac;
#[cfg(all(feature = "http3", any(target_os = "openbsd", target_os = "netbsd")))]
pub use ring::hmac;
