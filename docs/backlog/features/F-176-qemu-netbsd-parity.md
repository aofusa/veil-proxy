# F-176: QEMU の BSD スクリプトで NetBSD を FreeBSD/OpenBSD 並みに扱う

**状態: 対応中（feat/v080-limitations）**

## ギャップ（v0.7.0 の検証で実際に踏んだもの）

1. NetBSD x86_64 のコンソールがシリアルに出ない（`/boot.cfg` の `consdev=com0` が効いていない）
2. 異常終了後の fsck を自動で回復できない（全 OS 共通だが NetBSD x86_64 は 1 のため手作業が重い）
3. 既定 4GB で単体テストのリンクが OOM（`ld` が Killed、VM ごと落ちることもある）
4. NetBSD aarch64 の E2E が HTTP/3 抜き（B-62: libc の `_ALIGNBYTES` 誤りでテストクライアントの quinn-udp が panic）
5. 計測ハーネス（`tools/perf/freebsd/`）が FreeBSD 専用
6. `grow` が FreeBSD 専用
7. FreeBSD の capmode E2E に相当する OS 固有のセキュリティ検証が無い

## 改修

1. provision で `installboot` のコンソール指定と `/etc/ttys` を設定し、再起動後にシリアルの `login:` を確かめる。
   `screen`（QMP screendump）/ `sendkeys` コマンドを追加
2. NetBSD の `/` と `/work` を `log`（WAPBL）でマウント。全 OS 共通の `rescue`（シリアルでシングルユーザーを検出して fsck）
3. `VM_MEM_MB` の既定を OS/アーキ別に（NetBSD x86_64 = 7168）。単体テストのリンクを直列化
4. テストクライアント側で cmsg のアラインメントを正しく扱い、`full-netbsd-no-http3` への切り替えを廃止
5. `tools/perf/bsd/` に一般化（NetBSD/OpenBSD の nginx・syscall 計数・CPU プロファイル）
6. NetBSD aarch64 の `grow`
7. `bsd-security-e2e.sh`（FreeBSD capmode / NetBSD chroot + 特権降格 / OpenBSD pledge+unveil）
