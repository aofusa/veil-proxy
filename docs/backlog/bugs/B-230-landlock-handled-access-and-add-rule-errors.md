# B-230: Landlock の handled access に `MAKE_SYM`・`MAKE_SOCK`・`MAKE_FIFO`・`EXECUTE` が無く、`LANDLOCK_ADD_RULE` の失敗を無視している

**状態: 未修正（2026-10-27 起票。セキュリティレビュー SB-03）**

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

## 関連

- [B-219](B-219-seccomp-landlock-worker-threads-only.md)、[B-220](B-220-seccomp-missing-dup-unlink-mkdir.md)
