# F-82: ファジングのCI統合（長時間実行・Corpus永続化）

親: [F-52](F-52-cargo-fuzz-libfuzzer.md)

## 目的

F-52で導入されたファジング基盤を活用し、CI上でより長時間のファジングと成果物の自動保存を行う外部インフラ連携を整備する。

## 改修内容

- ASAN/TSAN ビルドコンテナでの長時間ファジング（nightly等のCIバッチジョブ）
- corpus の Artifact 保存・minimization（nightly）

## 受け入れ条件

- CI環境にて長時間ファジングが実行されること。
- corpus が CI の artifact 等として適切に保存・minimization されること。

## 対応結果（2026-10-05、完了）

`container-security-nightly.yml` に週次の `sanitizers` ジョブを追加。ASAN/TSAN の E2E カオス（`RUN_E2E_ASAN/TSAN=1`、`E2E_SANITIZER_BLOCKING=1`）と ASAN libFuzzer（600 秒/ターゲット）を回す。コーパスは `actions/cache` で回次間に持ち越し、`run_libfuzzer_asan.sh` に追加した `FUZZ_CMIN=1`（`cargo fuzz cmin`）で毎回最小化してから保存し、artifact にも添付する。

ワークフローは GitHub Actions 上でしか実行できないため、ローカルでは YAML の構文検証と、
同じ環境変数での `tools/container_security/run.sh` の実行（v0.7.0 リリース前検証）で確認した。
