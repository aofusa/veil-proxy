# B-382: 利用者だけを指定した権限降格が root のグループ（gid 0・補助グループ）を残していた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx の 2 回目のセキュリティスキャン（`CLAUDE-SECURITY-20261114-024333`）の候補から確認。

## 事象

- `system::drop_privileges` は `setgid` と補助グループの消去（`setgroups(0)`）を `drop_privileges_group` を
  指定したときだけ行っていた。`drop_privileges_user` だけを指定すると `setuid` だけが走り、gid 0 と root の
  補助グループ（FreeBSD の `wheel`・`operator` 等）が残った。プロセスを乗っ取られたとき（メモリ安全性の欠陥・
  Wasm の脱出等）、root のグループで読める鍵・デバイスが読める。
- 補助グループを外せなかったとき・降格の後も `setuid(0)` が通ったときも警告だけで動き続けていた。

## 対応

- 利用者だけを指定したときは、その利用者の主グループ（`getpwnam` の `pw_gid`）へ下ろす（名前の解決は
  従来どおり chroot・サンドボックスの前）。
- 降格するときは補助グループを常に外し、外せなければ降格の失敗にする（起動を止める）。
- 降格の後に `setuid(0)` が通れば降格の失敗にする。
- 文書: `docs/guide/routing.md`・`docs/guide/security.md`（英日）。配布物の設定（コンテナ・OpenBSD・NetBSD）は
  利用者とグループの両方を指定しているので変わらない。

## テスト

- `system::tests::user_only_privilege_drop_uses_the_users_primary_group`（名前の解決。root で走らせなくてよい形に
  分けた `resolve_privilege_ids_as_root`）。降格そのものは root を要するので単体試験にしない。
