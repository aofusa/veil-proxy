# B-82: NetBSD x86_64 の live イメージが初回起動でファイルシステム破損する

**優先度**: P2
**ステータス**: 完了（2026-08-29）
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
**x86_64 向けの NetBSD バイナリをビルドできなかった。**

## 原因

**`_create_overlay` によるルートディスクの拡張（+`GROW_GB`=24G）そのものが原因。**

NetBSD amd64 の live イメージは **MBR + disklabel** 構成で、ブートも
`NetBSD/x86 ffsv1 Primary Bootstrap`（BIOS 経路）を通る。qcow2 オーバーレイの
仮想サイズを 1.86GiB → 25.9GiB に変えると **SeaBIOS が申告するディスクの CHS
ジオメトリが変わり**、ルートパーティションの位置の解釈がずれる。その結果 fsck が
別の場所を FFS スーパーブロックとして読み、`UNEXPECTED INCONSISTENCY` になる。

イメージ自体は壊れていない。ダウンロードや `qemu-img convert` の問題でもない。

### 切り分け（実測）

同一の `base.qcow2` に対してオーバーレイの作り方だけを変えて比較した。

| オーバーレイ | 結果 |
|---|---|
| `base + 24G`（従来） | fsck 失敗 → ABORTING BOOT（**2/2 再現**） |
| **拡張なし（base と同サイズ）** | **fsck 通過 → マルチユーザ到達 → sshd 起動 → provision 完了** |

### aarch64 が無事だった理由

**インストール方式は x86_64 と同一**（両アーキとも配布の起動可能な生イメージを
qcow2 化してオーバーレイを作る）。違いは**ブート経路**だけで、aarch64 は
UEFI + GPT のため CHS ジオメトリに依存せず、ディスクを拡張しても位置がずれない。

> 本チケットの初版には「aarch64 は install ISO + sysinst、x86_64 は live イメージ」
> という方式差の表があったが、**これは誤り**。sysinst 経路は実機で言語選択メニュー
> のまま停止するため既に廃止済みで、現在は両アーキとも生イメージ経路に統一されている。

## 対処

`tools/qemu/bsd-vm.sh`（NetBSD x86_64 のみ。aarch64 と他 OS の挙動は変えない）:

1. **ルートディスクを拡張しない**（`_create_overlay` に x86_64 の分岐を追加）。
2. ビルドに必要な容量は **2 台目のディスク**で与える。`setup` で
   `scratch.qcow2`（`GROW_GB`）を作り、`up` が `index=1` で接続する
   （ゲストからは `ld1`。ラベルの無い素のディスクでは `a` が全体を覆う
   4.2BSD パーティションになるので `newfs -O2 /dev/rld1a`）。
3. `provision` の末尾で `_netbsd_init_scratch` が newfs + `/work` へマウントし、
   `/etc/fstab` に登録する。既存 FS が読めるうちは newfs しない冪等な実装。
4. ルート FS（1.8G、空き実測 ~340M）では足りないものを `/work` へ逃がす:
   - `/usr/pkg` → `/work/pkg`（シンボリックリンク）
   - `/var/db/pkgin` → `/work/pkgin`（同上）
   - `GUEST_ROOT` → `/work/veil-proxy`
   - `CARGO_HOME=/work/cargo` / `TMPDIR=/work/tmp`

`/work` の空きは 22G。

## 影響

解消。`packaging/output/veil-<ver>-x86_64-unknown-netbsd.tar.gz` を生成できる。

## 教訓

**MBR + BIOS 経路の x86 ディスクイメージは、qcow2 の仮想サイズを後から変えては
いけない。** UEFI + GPT のイメージ（NetBSD aarch64、FreeBSD の VM-IMAGE）では
起きないため、同じ手順が片方のアーキでだけ壊れる形で表面化する。容量が要るときは
ルートを広げるのではなく**別ディスクを足す**。
