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
- LibAFL への段階的移行検討
  - ※ASAN/TSAN ビルドコンテナでの長時間ファジング、および corpus の Artifact 保存・minimization（nightly）など、CI・外部インフラを要するタスクについては [F-82](F-82-fuzzing-ci-nightly.md) として未着手の別タスクへ分離した。

## 実行記録（2026-07-05）

4 ターゲットを各 60 秒（`-max_total_time=60`）ネイティブ実行:

| ターゲット | 結果 |
|-----------|------|
| `hpack_decode` | **クラッシュ検出 → [B-21](../bugs/B-21-hpack-huffman-decode-shift-panic.md)**（Huffman デコーダのシフト panic）。修正後は再現せず |
| `http2_frame_decode` | 219,102 runs / 61s、クラッシュなし |
| `http_header_validate` | 214,032 runs / 61s、クラッシュなし |
| `config_toml` | 55,970 runs / 61s、クラッシュなし |

クラッシュ入力は回帰テスト（`huffman::tests::test_huffman_decode_invalid_no_panic`）
として固定済み。

## 受け入れ条件

- [x] `cargo fuzz run hpack_decode` がクラッシュなく完了（B-21 修正後）
- コンテナスクリプトで CI/nightly から実行可能