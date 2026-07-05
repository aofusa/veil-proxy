# F-79: カバレッジ計測の常設化（cargo llvm-cov）

親: [F-72](F-72-security-testing-further-hardening.md) 項目 5 / [F-55](F-55-harness-hardening.md)。

## 目的

ファズ/ユニットのカバレッジを可視化し、テストの盲点を定量把握する。

## 改修案

- `cargo llvm-cov`（+ `cargo fuzz coverage`）でカバレッジを生成し、
  `tools/container_security/lib/report.sh` の集約へ組み込む。
- コーパス（[F-80](F-80-regression-corpus.md)）に対する fuzz カバレッジと
  ユニット/統合テストのカバレッジを分けてレポート。

## 受け入れ条件

- docker のみで lcov/HTML カバレッジが生成され、suite サマリに率が載る。
