# B-250: サンドボックスの capability の削除が bounding set だけで、プロセスが持つ effective/permitted は落とさない

**状態: 未修正（2026-10-27 起票）**

## 出典

2026-10-27 のセキュリティレビュー 2 回目の SB-10。

## 事象

`src/security.rs` L2226〜L2290（`apply_capabilities`・`drop_capability`）は `prctl(PR_CAPBSET_DROP, cap)` だけを呼ぶ。bounding set は
**`execve` 時に permitted を制限する** ものなので、veil 自身が今持っている effective/permitted/inheritable の capability には効かない。
root で起動して `[security.privilege_drop]` を使わない構成（コンテナの `user 0:0` など）では、`[security.sandbox].drop_capabilities` を
設定しても veil は全 capability を持ったまま動く。`drop_capability` の失敗は `warn!` も出さずに無視する（戻り値を捨てる）。

`report_security_status` は capability の状態を表示しないので運用者は気づかない。

## 改修案

- `capset(2)`（`libc::syscall(SYS_capset, ..)`、`_LINUX_CAPABILITY_VERSION_3`）で permitted・effective・inheritable から同じ集合を落とし、
  `PR_CAP_AMBIENT_CLEAR_ALL` も呼ぶ。bounding set の削除は維持する。失敗は `strict` 相当のモードでは起動エラー、それ以外は `warn!`。
- `report_security_status` に `/proc/self/status` の `CapEff`/`CapBnd` を出す。
- E2E（`tests/security`）: サンドボックス起動後の `CapEff` が設定どおりに落ちている。

## 関連

- [B-219](B-219-seccomp-landlock-worker-threads-only.md)、[B-230](B-230-landlock-handled-access-and-add-rule-errors.md)、
  [F-282](../features/F-282-security-review-followup-hardening.md)
