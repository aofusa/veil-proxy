# B-108: NetBSD で chroot_dir と特権降格を併用すると起動に失敗する

**状態: 完了（feat/v080-limitations）**

## 事象

NetBSD で `[security] chroot_dir` と `drop_privileges_user` / `drop_privileges_group` を併用すると、
起動時に `Failed to drop privileges: Group 'nobody' not found` で終了する（ワーカーが動かない）。
F-176 で追加した OS 固有サンドボックスの E2E（`bsd-vm.sh netbsd aarch64 security-e2e`）で発見。

## 原因

起動シーケンスは「chroot(2) → … → 特権降格」の順で、降格の中で `getgrnam` / `getpwnam` を呼んでいた。
chroot 後は新しいルートの `/etc/group`・`/etc/passwd` を参照するため、chroot 内にそれらを置かない限り
名前を解決できない。chroot は setuid より前に行う必要がある（先に setuid すると chroot が EPERM）ので、
順序を入れ替えることはできない。

## 改修

`system::resolve_privilege_ids` で利用者・グループ名を chroot・サンドボックスの**前**に ID へ解決し、
`drop_privileges` は解決済みの ID で `setgid` / `setgroups` / `setuid` だけを行う。全 OS 共通の変更
（FreeBSD capsicum / OpenBSD unveil 等の後で名前解決が拒否される構成も同じ理由で安全になる）。
