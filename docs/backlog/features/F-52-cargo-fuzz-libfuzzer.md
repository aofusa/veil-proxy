# F-52: cargo-fuzz / LibFuzzer 統合

## 目的

HTTP/2 HPACK、設定パーサ等の **メモリ安全境界** を継続的ファジングで検証する。

## 現状

- `src/lib.rs` を追加し `cargo fuzz` から `veil` クレートをリンク可能にした。
- `fuzz/fuzz_targets/hpack_decode.rs` — HPACK デコーダ
- `fuzz/fuzz_targets/config_toml.rs` — `test_config_file` 経由の TOML 検証
- `fuzz/fuzz_targets/http2_frame_decode.rs` — HTTP/2 フレームデコーダ
- `fuzz/fuzz_targets/http_header_validate.rs` — HTTP/1 ヘッダー境界（`fuzz_api`）
- コンテナ実行: `tools/container_security/fuzz/run_libfuzzer.sh`（4 ターゲットループ）

## 改修内容（残件）
- ASAN/TSAN ビルドコンテナでの長時間ファジング
- corpus の Artifact 保存・minimization（nightly）
- LibAFL への段階的移行検討

## 受け入れ条件

- `cargo fuzz run hpack_decode -- -runs=1000` がクラッシュなく完了
- コンテナスクリプトで CI/nightly から実行可能