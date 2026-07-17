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
