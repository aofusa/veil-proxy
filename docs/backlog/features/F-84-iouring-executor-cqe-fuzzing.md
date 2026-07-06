# F-84: io_uring executor 擬似 CQE 注入ファジング

出典: [container_security_review_report.md](../../artifacts/container_security_review_report.md) 提案1。親: [F-52](F-52-cargo-fuzz-libfuzzer.md) / [F-07](fuzzing-chaos-security.md)。

## 目的

既存の fuzz ターゲットはステートレスなプロトコルデコーダ・設定パーサーに偏っており、
コアランタイム（`src/runtime/` の io_uring 実装）の非同期境界が直接ファジングされていない。
カーネルを介さずに、Fuzzer が生成したバイト列を **擬似 CQE（Completion Queue Event）** として
executor の op テーブル・ディスパッチ経路へ直接注入し、異常な `res` 値（負のエラーコード・
想定外サイズ）、`user_data` の不整合（stale 世代・無効 index・センチネル衝突）、完了順序の
逆転・重複などで panic や状態不整合（スロットリーク・二重解放・ガード誤発火）が起きないことを
機械的に検証する。

## 実装済み（2026-07-06）

- `src/runtime/executor.rs::fuzz_op_table_sequence(bytes)`（`pub(crate)`）: バイト列を
  「alloc / CQE(追跡 id・任意 res) / CQE(偽造 user_data) / detach / take / peek / remove /
  set_waker」の操作列として解釈し、実リング無しで `OpTable`（スラブ + 世代）の完了
  ディスパッチ経路を駆動。不変条件（panic なし・detach ガード exactly-once・pending detach は
  遅延 CQE でちょうど 1 回・全スロット解放）を assert 検査。
- 公開エントリ `src/fuzz_api.rs::io_uring_executor_smoke(bytes)`。
- fuzz ターゲット `fuzz/fuzz_targets/io_uring_executor.rs` + `fuzz/Cargo.toml` の `[[bin]]`。
- `tools/container_security/fuzz/run_libfuzzer.sh` の既定 `FUZZ_TARGETS` へ配線。
- 回帰単体テスト 2 件（代表操作列 + 決定的 LCG によるミニファザー 256 列）。
- ローカル走行: `io_uring_executor -runs=20000` クラッシュなし完走。

## 改修内容（当初案）

- `src/fuzz_api.rs` に `io_uring_executor_smoke(bytes)` を追加。バイト列を
  「alloc / on_cqe(任意 user_data・res) / detach(guard) / take / peek / remove / set_waker」の
  操作列として解釈し、`OpTable` 相当の公開経路（`alloc_op`/`process_cqe`/`detach_op_no_cancel`/
  `take_op_result`/`peek_op_result`/`remove_op`）を実リング無しで駆動する。
- 不変条件を assert で検査:
  - 任意操作列で panic しない。
  - alloc した op はいずれかの経路（take/remove/detach+CQE）で必ず解放でき、スロットがリークしない。
  - detach 済み op のガードは **ちょうど 1 回** だけ実行される（0 回=リーク、2 回=二重解放）。
  - stale 世代の CQE が新 op に影響しない（B-07 の回帰性質）。
- fuzz ターゲット `fuzz/fuzz_targets/io_uring_executor.rs` + `fuzz/Cargo.toml` の `[[bin]]`。
- `tools/container_security/fuzz/run_libfuzzer.sh` の既定ターゲットへ配線。
- 回帰単体テスト（代表的な操作列で不変条件を `cargo test` で実証）。

## 受け入れ条件

- [x] `cargo fuzz run io_uring_executor` が短時間走行でクラッシュ無く完走すること
      （ローカルは `cargo build -p veil-fuzz` + `-runs=20000` で実証）。
- [x] 回帰単体テストが `cargo test` で通過すること。
- [x] container_security の libFuzzer フェーズに含まれること。

## 依存・リスク

- 実リング（カーネル）は使わないため、SQE 提出・restriction 検証は対象外（F-86 が補完）。
- executor 内部 API の可視性拡大は fuzz/テスト到達に必要な最小限（`pub(crate)` 優先）に留める。
