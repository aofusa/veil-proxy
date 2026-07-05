# F-78: OSS-Fuzz 連携の検討

親: [F-72](F-72-security-testing-further-hardening.md) 項目 4 / [F-52](F-52-cargo-fuzz-libfuzzer.md)。優先度 P3（外部インフラ依存）。

## 目的

既存 fuzz ターゲット（hpack/frame/header/config/wasm_abi/wasm_host_abi）を OSS-Fuzz へ
載せ、継続ファジングとクラッシュ自動起票を外部インフラで得る。

## 改修案

- `projects/veil/` の OSS-Fuzz プロジェクト定義（Dockerfile・build.sh・project.yaml）を用意。
- `cargo fuzz` ターゲットを OSS-Fuzz のビルド規約へ適合。
- クラッシュ通知先・回帰コーパス還流（[F-80](F-80-regression-corpus.md)）と接続。

## 受け入れ条件

- OSS-Fuzz のローカルビルド（`infra/helper.py build_fuzzers`）が通ること。
- 上流申請の可否を判断する材料が揃うこと。
