# B-472: deb を上げると /etc/veil/config.toml が誰でも読める 0644 のまま残った

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「deb upgrades leave /etc/veil/config.toml world-readable (0644 root:root), and the 0.8.x migration guide tells admins to
copy their secrets into it」
から確認。

## 事象

- deb は `/etc/veil/config.toml` を conffile として 0644 root:root で入れ、postinst は新しく入れたとき（前の版が空）だけ
  root:veil 0640 にしていた。0.8.x（設定は `/var/etc/veil`）から上げると conffile は上げるときに初めて入るので 0644 の
  まま残り、案内どおりに `[admin] secret` 等を書き移すと他の利用者が読めた。dpkg が手の入っていない conffile を入れ替え
  たときも 0644 に戻る（rpm は `%attr(0640,root,veil)` で起きない）。

## 対応

- postinst（`packaging/scripts/postinstall.sh`）は、新しく入れたときに加えて、設定がパッケージの権限（root:root 0644）
  のままなら上げるときも root:veil 0640 にする。利用者が自分で決めた所有者・権限は変えない。

## テスト

- `stat -c '%a %u:%g'` の形を手元で確かめ、`sh -n` で構文を確かめた（パッケージの試験 `test-package.sh` は Docker を
  使うのでリリースの経路で回る）。
