# B-473: BSD の VM の作成で、FreeBSD のイメージを平文の http で確かめずに取り、OpenBSD は署名の無い sets も入れていた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「FreeBSD VM image fallback downloads over plain HTTP with no checksum or signature check (default path for the archived
14.3-RELEASE)」と「OpenBSD autoinstall answer file falls back to plain HTTP and continues without signify verification of the
install sets」
から確認。

## 事象

- `tools/qemu/bsd-vm.sh` の FreeBSD は、本家のミラーに無いリリース（既定の 14.3-RELEASE は 2026-10 から）を
  `http://ftp-archive.freebsd.org` から平文で取り、ハッシュも署名も見ずに VM のルートディスクにしていた。この VM で
  ビルドしたバイナリは BSD のパッケージへ取り込まれる。
- OpenBSD の autoinstall の応答は「SHA256.sig が無いので確かめずに続けるか」に yes を答えていた。https から http へ
  落とされた経路で署名を消されると、改変した sets が入った。

## 対応

- FreeBSD: アーカイブも https（`https://archive.freebsd.org/old-releases`。ftp-archive の名前は証明書と合わない）で
  取り、取った `.xz` は同じディレクトリの `CHECKSUM.SHA256`（https）と照らしてから展開する。合わない・載っていない
  なら消して止める。
- OpenBSD: 「確かめずに続ける」の答えを消し、署名が無ければ既定の答え（no）でインストールを止める（http への切り替えは、
  sets を signify で確かめるので残す）。

## テスト

- `_verify_freebsd_image` を `file://` の URL で試した（一致・不一致・載っていない）。14.3-RELEASE の amd64・aarch64 の
  イメージが https のアーカイブの `CHECKSUM.SHA256` に載っていることを確かめた。
