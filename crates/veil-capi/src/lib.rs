// F-291: C API の本体は veil-capi-core（rlib）にある。依存の rlib の `#[no_mangle] pub extern "C"` も cdylib・
// staticlib から公開される（全記号の公開は tests/api/library_e2e.sh c-kit の nm の検査で確かめる）。
//! The C library `libveil` (`libveil.so` / `libveil.a` / `veil.dll` with `include/veil.h`).
//!
//! The functions live in `veil-capi-core`; this crate only builds the library files.
pub use veil_capi::*;
