# F-85: E2E カオステストへの ASAN/TSAN 統合

出典: [container_security_review_report.md](../../artifacts/container_security_review_report.md) 提案2。親: [F-71](F-71-asan-corpus-fuzzing.md) / [F-53](F-53-chaos-engineering-expansion.md)。

## 目的

Sanitizer（ASAN/TSAN/LSAN）は現状 libFuzzer ターゲット（プロトコルパーサー）にのみ適用されて
おり、プロキシ全体を通したメモリアクセス違反・データ競合（io_uring 固有の Future Drop 時
UAF 等）を実行レベルで検出できていない。`chaos_load.sh` の E2E カオス負荷（高並行 HTTP/HTTPS・
接続チャーン・SIGHUP リロード）を **sanitizer ビルドの Veil バイナリ** で実行するモードを
オーケストレータに新設し、カーネルと密に連携する経路（Future キャンセル・切断・リロード）で
UAF・リークが発生していないかを捕捉する。

## 改修内容

- sanitizer ビルドの Veil コンテナイメージを作る仕組み（例: `RUSTFLAGS="-Zsanitizer=address"`
  + nightly ビルドの Dockerfile ステージ or 専用ビルドスクリプト）。
- `tools/container_security/run.sh` に `RUN_E2E_ASAN=1` / `RUN_E2E_TSAN=1`（既定 0）を新設し、
  sanitizer ビルドの Veil で起動 → `chaos_load.sh`（高並行 + SIGHUP + 切断チャーン）を実行 →
  コンテナログの sanitizer レポート（`ERROR: AddressSanitizer` / `WARNING: ThreadSanitizer`）を
  検査して pass/fail をレポートへ集約する。
- ASAN ランタイムは seccomp/Landlock と干渉し得るため、sanitizer 実行時のみ緩和した
  実行プロファイル（`ASAN_OPTIONS`、必要なら seccomp unconfined）を明示・文書化する。

## 受け入れ条件

- [ ] `RUN_E2E_ASAN=1 ./tools/container_security/run.sh` で sanitizer ビルドの Veil に対する
      カオス負荷が完走し、レポートに sanitizer 検査結果が記録されること。
- [ ] sanitizer エラー検出時にフェーズが fail としてサマリへ集約されること。
- [ ] README（container_security）へ実行方法・注意点（nightly・seccomp 緩和）を追記すること。

## 依存・リスク

- nightly toolchain + `-Zbuild-std` 相当のビルド時間増（nightly 専用フェーズ、既定 SKIP）。
- TSAN はランタイム前提（pthread 計装）により偽陽性・大幅減速があり得る。まず ASAN を主軸にする。
