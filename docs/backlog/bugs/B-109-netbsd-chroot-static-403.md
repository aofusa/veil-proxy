# B-109: NetBSD の chroot_dir 下で静的配信がすべて 403 になる

**状態: 完了（feat/v080-limitations）**

## 事象

NetBSD で `[security] chroot_dir` を設定し、証明書と静的ファイルを chroot 前後の両方から同じパスで
読めるように chroot 外の同じパスへシンボリックリンクを張る構成（証明書は chroot 前に、静的ファイルと
証明書のリロードは chroot 後に読むため、自然にこの形になる）で、`File` ルートの要求がすべて
`403 Forbidden` になる。F-176 の OS 固有サンドボックス E2E（`bsd-vm.sh netbsd x86_64 security-e2e`）で
B-108 を直した後に発見。

## 原因

`File` ルートの `resolved_backend`（F-159）と、その封じ込め検査に使う base_path の canonical 形
（F-145 / B-64 の `canonical_base_memoized`）は、設定ロード時＝**chroot 前**に解決される。
シンボリックリンクを辿った chroot 外の実体パス（`/var/tmp/.../chroot/data/www`）が base になる一方、
要求時のファイル解決は chroot 後の新しいルート基準（`/data/www/index.html`）になるため、
`sendfile_base_contains` の包含判定が常に偽になっていた。

## 改修

`config::reresolve_routes_after_chroot` で、chroot に成功した直後に canonical 形と is_dir のメモを捨てて
各ルートの `resolved_backend` を新しいルート基準で作り直す（起動時に 1 回だけ。ホットパスは不変）。
SIGHUP の設定リロードはリロードスレッド（chroot 後）で解決するので元々正しい。

## 検証

- 単体: `config::f159_resolved_backend_tests::reresolve_routes_after_chroot_drops_stale_canonical_base`
  （シンボリックリンクの差し替えで「見え方の変化」を再現し、メモを捨てて解決し直すことを確認）
- NetBSD 10.1 x86_64 / aarch64 の `security-e2e`（chroot + nobody への降格 + 静的配信 + 証明書リロード）
