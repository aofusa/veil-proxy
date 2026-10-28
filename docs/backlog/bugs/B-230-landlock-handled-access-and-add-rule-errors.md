# B-230: Landlock の handled access に `MAKE_SYM`・`MAKE_SOCK`・`MAKE_FIFO`・`EXECUTE` が無く、`LANDLOCK_ADD_RULE` の失敗を無視している

**状態: 修正済み（v0.10.0）**

## 出典

2026-10-27 のセキュリティレビュー（v0.9.2 の `src/security.rs::apply_landlock` の読解）。

## 事象

- `handled_access_fs` に `LANDLOCK_ACCESS_FS_MAKE_SYM`・`MAKE_SOCK`・`MAKE_FIFO`・`MAKE_CHAR`・`MAKE_BLOCK`・`EXECUTE` が無い。Landlock は
  handled に無いアクセスを**制限しない**ので、書き込みを許したディレクトリの外でもシンボリックリンク・ソケット・FIFO を作れる
  （`EXECUTE` は seccomp の `PROT_EXEC` の検査で実害が薄い）。
- `LANDLOCK_ADD_RULE` の戻り値を見ず、存在しないパスは黙ってスキップする。fail-closed（そのパスへのアクセスが拒否される）ではあるが、
  運用者が設定ミスに気づけない。

## 改修案

- handled に上記を足す（ABI v1 で全部ある。`EXECUTE` は Wasm の JIT と無関係なファイルの実行だけ）。UDS のリスナー（`MAKE_SOCK`）・
  `[server].listen_unix` のディレクトリには `MAKE_SOCK` を許す。
- `LANDLOCK_ADD_RULE` の失敗は `warn!` で出し、存在しないパスは起動時の検証で `error!` にする（設定の検証の一部）。
- `docs/guide/security.md`（英・日）の Landlock の説明を合わせる。

## 受け入れ条件

- E2E（seccomp/Landlock 有効）: 許可外のディレクトリで `symlink`・UDS の `bind` が EACCES になり、`listen_unix` のソケットは作れる。

## 修正（v0.10.0）

- Landlock の handled に `EXECUTE`・`MAKE_CHAR`・`MAKE_SOCK`・`MAKE_FIFO`・`MAKE_BLOCK`・`MAKE_SYM` を足した（ABI v1 から全部ある）。`MAKE_SOCK` だけ `landlock_write_paths` に許し、他はどのパスにも許さない。
- ルールを足すときは、開けないパスと `landlock_add_rule` の失敗を `warn!` で出す。通常のファイルのパスにはファイルに意味のあるアクセス（READ_FILE・WRITE_FILE・TRUNCATE・IOCTL_DEV）だけを渡す（以前は READ_DIR 等を含めて EINVAL になり、ファイルを列挙したルールは黙って効いていなかった）。
- 試験: `landlock_b230_tests::landlock_restricts_special_files`（自分を子プロセスとして起動して Landlock を掛け、許可外のシンボリックリンク・UDS の bind が EACCES、許可内では UDS と通常のファイルは作れてシンボリックリンク・FIFO は EACCES）。
- `docs/guide/security.md`（英日）に説明を足した。

## 関連

- [B-219](B-219-seccomp-landlock-worker-threads-only.md)、[B-220](B-220-seccomp-missing-dup-unlink-mkdir.md)
