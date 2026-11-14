# B-398: ACME の起動時の書き込みが、予測できる一時ファイルの名前でシンボリックリンクを辿っていた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx の 2 回目のセキュリティスキャン（`CLAUDE-SECURITY-20261114-024333`）の指摘 F33 から確認。

## 事象

- `acme::bootstrap` は `drop_privileges` の前（root）に走り、`write_atomic` で仮の証明書・鍵を書く。一時ファイルは
  `<dir>/.<file>.tmp<pid>` を `create(true).truncate(true)` で開いており、`O_EXCL`・`O_NOFOLLOW` が無かった。
- `docs/guide/acme.md` は証明書・鍵・`storage_dir` のディレクトリを降格後の利用者が書けるようにと求めるので、
  特権の無い利用者がその名前にシンボリックリンクを置くと、root がそれを辿って任意のファイルを書き換える。
  `create_dir` もパスで `chmod` していたので、置き換えたリンクの先の権限を変えた。

## 対応

- `create_temp` で、一時ファイルを毎回新しい名前（pid・連番・時刻）で `create_new`（`O_EXCL`）と `O_NOFOLLOW` で
  作る。名前が埋まっていれば 16 回まで別の名前でやり直す。
- `create_dir` は `O_DIRECTORY | O_NOFOLLOW` で開いたディレクトリへ `fchmod` する（運用で置いたディレクトリへの
  シンボリックリンクはそのまま使い、権限を変えない）。
- 利用者ガイド（`acme.md`・`ja`）に書いた。

## テスト

- `acme::tests::temp_files_do_not_follow_planted_symlinks`。
