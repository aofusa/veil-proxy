# F-82: ファジングのCI統合（長時間実行・Corpus永続化）

親: [F-52](F-52-cargo-fuzz-libfuzzer.md)

## 目的

F-52で導入されたファジング基盤を活用し、CI上でより長時間のファジングと成果物の自動保存を行う外部インフラ連携を整備する。

## 改修内容（未着手）

- ASAN/TSAN ビルドコンテナでの長時間ファジング（nightly等のCIバッチジョブ）
- corpus の Artifact 保存・minimization（nightly）

## 受け入れ条件

- CI環境にて長時間ファジングが実行されること。
- corpus が CI の artifact 等として適切に保存・minimization されること。
