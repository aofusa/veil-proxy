# B-474: Windows で WASM のエンジンの再利用の試験が、mtime を写したコピーへの置き換えを見分けられずに失敗していた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max の確認で、aofish（Windows）の `cargo test --lib --features full-windows` から見つけた
（main の 92197f25 から。Windows の CI は単体試験を回していない）。

## 事象

- `config::loaded::b383_wasm_section_tests::b404_compiled_engine_is_reused_only_without_changes` は、モジュールを
  `std::fs::copy` した同じ中身のファイルで置き換え（`rename`）、変わったと判定されることを確かめていた。
- ファイルの印（`wasm::source::FileStamp`）は unix では inode・ctime を含むが、Windows では大きさと mtime だけで、
  `std::fs::copy`（CopyFileExW）は mtime を写すので同じ印になり、「expected a change, got the current engine」で落ちた。
- 印を見るのは seccomp で実行可能なメモリを断った Linux のリロード（`reload_wasm_engine`）だけで、Windows は
  リロードのたびにエンジンを作り直すので、動作の不具合ではない。

## 対応

- 試験の「置き換えたファイルを見分ける」部分を unix だけにした（理由をコメントに書いた）。

## テスト

- aofish の `cargo test --lib --features full-windows`（1204 件）と自己完結型 E2E（50 件）が通ることを確かめた。
