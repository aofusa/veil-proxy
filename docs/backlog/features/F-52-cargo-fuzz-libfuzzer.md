# F-52: cargo-fuzz / LibFuzzer 統合

## 目的

HTTP/2 HPACK、設定パーサ等の **メモリ安全境界** を継続的ファジングで検証する。

## 現状

- `src/lib.rs` を追加し `cargo fuzz` から `veil` クレートをリンク可能にした。
- `fuzz/fuzz_targets/hpack_decode.rs` — HPACK デコーダ
- `fuzz/fuzz_targets/config_toml.rs` — `test_config_file` 経由の TOML 検証
- コンテナ実行: `tests/container_security/fuzz/run_libfuzzer.sh`

## 改修内容（残件）

- HTTP/2 フレームデコーダ、HTTP/1 パーサ境界のターゲット追加
- ASAN/TSAN ビルドコンテナでの長時間ファジング
- corpus の Artifact 保存・minimization（nightly）
- LibAFL への段階的移行検討

## 受け入れ条件

- `cargo fuzz run hpack_decode -- -runs=1000` がクラッシュなく完了
- コンテナスクリプトで CI/nightly から実行可能