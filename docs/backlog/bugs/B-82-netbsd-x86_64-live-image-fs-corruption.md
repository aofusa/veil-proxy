# B-82: NetBSD x86_64 の live イメージが初回起動でファイルシステム破損する

**優先度**: P2
**ステータス**: 未修正（x86_64 のみ。aarch64 は正常）
**発見日**: 2026-08-28（x86_64 向け配布バイナリのビルド中）

---

## 事象

`tools/qemu/bsd-vm.sh netbsd x86_64 setup && up && provision` を実行すると、
**初回起動でルートファイルシステムの自動チェックに失敗し、マルチユーザ起動が中断する。**

```
/dev/rld0a: UNEXPECTED INCONSISTENCY; RUN fsck_ffs MANUALLY.
Automatic file system check failed; help!
ERROR: ABORTING BOOT (sending SIGTERM to parent)!
Enter pathname of shell or RETURN for /bin/sh:
```

sshd が起動しないため `provision` の SSH 到達待ちが必ずタイムアウトし、
**x86_64 向けの NetBSD バイナリをビルドできない。**

## 再現性

**再現する（2/2）。** 1 回目の失敗後、`disk.qcow2`（オーバーレイ）を削除して
**キャッシュ済みの `base.qcow2` から作り直しても同じ症状**になった。
一時的な破損ではなく、この経路が構造的に壊れたイメージを作っている。

## aarch64 との差

**NetBSD aarch64 は正常に動く**（本セッションでもビルド・E2E・成果物取得まで成功）。
両者はインストール方式が異なる（`packaging/README.md` の F-140 注記）:

| アーキ | 方式 |
|---|---|
| aarch64 | install ISO + `sysinst` をシリアルから自動操作（`netbsd-autoinstall.py`） |
| **x86_64** | **起動可能な `-live.img.gz`（生イメージ）をそのまま qcow2 化** |

**疑わしいのは x86_64 側の live イメージ経路**である。生イメージを qcow2 へ変換し
`GROW_GB` 分リサイズしているが、ファイルシステムの拡張やクリーンアンマウント状態が
担保されていない可能性が高い。

## 試した回復策

コンソール（telnet）から single-user シェルに入り `fsck -y` → `exit` を実行した。
VM の再起動までは進んだが **sshd は上がらず**、以降コンソールは provision スクリプトが
掴んでいるため追加の観測ができなかった。**回復策としては不十分。**

## 影響

- **`packaging/output/veil-<ver>-x86_64-unknown-netbsd.tar.gz` が生成できない。**
- FreeBSD x86_64・OpenBSD x86_64・全 aarch64 は影響なし。

## 調査の起点

1. `bsd-vm.sh` の netbsd x86_64 `setup` が live イメージをどう変換・リサイズしているか。
   変換直後の `fsck_ffs -n` でイメージが最初から不整合かどうかを確認する
   （最初から壊れているなら変換手順、起動後に壊れるならリサイズか書き込み経路）。
2. `GROW_GB` によるリサイズを行わずに（= 素の live イメージのまま）起動して再現するか。
   OpenBSD で `GROW_GB` を小さくすると別の壊れ方をした前例があるため、
   サイズ変更が関与している可能性を先に潰す。
3. aarch64 と同じ `sysinst` 自動インストール方式へ x86_64 も寄せる（方式差の解消）。
