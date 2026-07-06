# F-52: cargo-fuzz / LibFuzzer 統合

## 目的

HTTP/2 HPACK、設定パーサ等の **メモリ安全境界** を継続的ファジングで検証する。

## 現状

- `src/lib.rs` を追加し `cargo fuzz` から `veil` クレートをリンク可能にした。
- `fuzz/fuzz_targets/hpack_decode.rs` — HPACK デコーダ
- `fuzz/fuzz_targets/config_toml.rs` — `test_config_file` 経由の TOML 検証
- `fuzz/fuzz_targets/http2_frame_decode.rs` — HTTP/2 フレームデコーダ
- `fuzz/fuzz_targets/http_header_validate.rs` — HTTP/1 ヘッダー境界（`fuzz_api`）
- `fuzz/fuzz_targets/http_request_smuggling.rs` — **リクエストスマグリング分類（B-23 デシンク
  防御）**。任意ヘッダーブロックを `classify_request_framing` に通し、panic なし + 反デシンク
  不変条件（CL+TE は必ず拒否）を検査（2026-07-06 追加）
- コンテナ実行: `tools/container_security/fuzz/run_libfuzzer.sh`（5 ターゲットループ + WASM 2）

## 追加実装（2026-07-06）

- **スマグリング分類ファジングターゲットを追加**（`http_request_smuggling`）。`fuzz_api` に
  `http_request_smuggling_smoke` を新設し、CL/TE の任意組み合わせで
  `classify_request_framing` が panic せず、CL+TE を必ず拒否する（`Ok` になれば assert 失敗＝
  デシンクバイパス検出）ことを継続検証する。回帰単体
  `smuggling_tests::http_request_smuggling_smoke_handles_arbitrary_input` も追加。
- `run_libfuzzer.sh` の既定ターゲットへ配線。

## LibAFL 移行の判断（2026-07-06）

- **結論: 現時点では libFuzzer（cargo-fuzz）を継続し、LibAFL 移行は見送る（deferred）。**
- 根拠:
  - 既存 6 ターゲットは cargo-fuzz + libFuzzer で安定稼働し、B-21（HPACK Huffman panic）を
    実検出した実績がある。ローカル短時間実行・OSS-Fuzz（[F-78](F-78-differential-testing.md)）
    連携も libFuzzer 互換で確立済み。
  - LibAFL の主な優位点（多コア並列・カスタムオブザーバ・永続コーパスの高度な管理）は、
    **長時間・大規模 nightly 実行基盤**が前提であり、その受け皿は
    [F-82](F-82-fuzzing-ci-nightly.md)（未着手：長時間実行・Corpus 永続化）である。基盤が整う
    前にランナーだけ移行してもカバレッジ上の利得は小さく、ハーネス二重化の保守コストが勝る。
  - したがって LibAFL 移行は F-82 の CI 基盤整備とセットで再評価する（本チケットからは切り離す）。

## 改修内容（残件）
- LibAFL 移行は F-82（nightly CI 基盤）とセットで再評価（上記「LibAFL 移行の判断」参照）。
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