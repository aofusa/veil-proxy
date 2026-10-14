# B-125: API ライブラリの C・Python 版とサンプル・試験用の制御プレーンが Linux 以外でビルドできなかった

**状態: 完了（feat/cross-platform-api-vds-e2e）**

## 事象

- `veil-capi`（C）と `veil-python` が macOS・BSD・Windows でビルドできなかった（veil の build.rs が
  `--features epoll is only meaningful on Linux` で止める）。
- `examples/api/rust-showcase`・`rust-grpc` が Windows でビルドできなかった（`veil::veil_alloc` は unix 専用）。
- VDS の参照制御プレーン（Rust 版、`tools/vds/server`）が Windows でビルドできなかった（`tokio::signal::unix`）。
- Windows で `veil::tokio` に未使用関数の警告（`io_enabled_in_context`。使うのは unix だけの `net`）。

## 原因

- `veil-capi`・`veil-python` が veil の `epoll` feature を無条件に指定していた（F-193/F-194 の雛形のまま。コンテナ向けの
  readiness reactor を選ぶ意図だが、`epoll` は Linux でだけ指定できる）。
- サンプルは unix の `veil-alloc` を前提にしていた。

## 修正

- `epoll` は `[target.'cfg(target_os = "linux")'.dependencies]` でだけ足す。他の OS は既定の reactor（kqueue・WSAPoll）。
  `veil-capi` の使われていなかった `reactor` feature は削除した。
- サンプルの `#[global_allocator]` は `cfg(unix)`（Windows はシステムのアロケータ）。
- `tools/vds/server` の終了シグナルは unix で SIGTERM + Ctrl-C、それ以外は Ctrl-C。
- `io_enabled_in_context` は `cfg(unix)`。

## 試験

- macOS・FreeBSD・OpenBSD・NetBSD・Windows で `tests/api/library_e2e.sh` の C・Python が通過（F-203）。
