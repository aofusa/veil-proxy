//! io_uring 完了ベースのランタイムバックエンド（F-120 Phase 1）。
//!
//! `veil_rt_uring` cfg（build.rs 発行、`target_os = "linux"` かつ
//! `not(feature = "epoll")`）が有効な場合にのみコンパイルされる。
//!
//! ここに含まれるファイルは元々 `src/runtime/` 直下にあったものを
//! そのまま移動しただけで、ロジックは無変更（`git diff --find-renames` 参照）。
//! 公開パス（`crate::runtime::ring` / `crate::runtime::executor` /
//! `crate::runtime::tcp` / `crate::runtime::timer` / `crate::runtime::splice`）は
//! 親モジュール（`src/runtime/mod.rs`）の re-export により維持される。

pub mod executor;
pub mod ring;
pub mod splice;
pub mod tcp;
pub mod timer;
/// HTTP/3 向けパイプライン化 `IORING_OP_RECVMSG` 受信（F-124 → F-130 C1）。
/// HTTP/3 専用（`udp_send` は `crate::udp` を参照）のため http3 ゲート。
#[cfg(feature = "http3")]
pub mod udp_recv;
/// HTTP/3 向けパイプライン化 `IORING_OP_SENDMSG` 送信（F-130 C3）。
#[cfg(feature = "http3")]
pub mod udp_send;
