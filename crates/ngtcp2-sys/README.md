# ngtcp2-sys

veil 向け **ngtcp2** の低レベル FFI。

- ソース: 公式 <https://github.com/ngtcp2/ngtcp2>（タグは `build.rs` の `NGTCP2_VERSION`）
- TLS: **aws-lc-sys**（`ngtcp2_crypto_boringssl`、`AWS_LC_SYS_NO_PREFIX=1` と整合）
- バインディング: `src/bindings.rs`（再生成: `cargo build -p ngtcp2-sys --features overwrite`）
