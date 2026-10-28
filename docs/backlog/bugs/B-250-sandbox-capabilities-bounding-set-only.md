# B-250: サンドボックスの capability の削除が bounding set だけで、プロセスが持つ effective/permitted は落とさない

**状態: 修正済み（v0.10.0）**

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

## 修正（v0.10.0）

- `src/security.rs` の `apply_capabilities` は落とす集合をビットマスクにし、`caps::drop_on_all_threads` で **全スレッド** から落とす: 呼び出したスレッドは bounding set（`PR_CAPBSET_DROP`。effective の CAP_SETPCAP が要るので先）・ambient（`PR_CAP_AMBIENT_LOWER`）・`capset(2)`（v3）で effective・permitted・inheritable から落とし、先に作られたスレッド（ログ・リロード・ヘルスチェック・VDS・ACME 等）には glibc が setuid を配るのと同じく実時間シグナル（`SIGRTMIN+3`、handler は syscall と atomics だけ）を `tgkill` で送って各自に落とさせ、全員の応答を待つ（送っている間に増えたスレッドが無くなるまで繰り返す。5 秒で応答が揃わなければエラー）。
- capset の失敗・応答の欠けは起動エラー（`allow_security_failures` で続行）。bounding set だけの拒否（CAP_SETPCAP が無い）は `warn!`。
- `report_security_status` が `/proc/self/status` の `CapEff`・`CapBnd` を出す。
- 試験: 単体 `capabilities_b250_tests::capabilities_are_dropped_on_every_thread`（root のときだけ。先に 3 本スレッドを作ってから `CAP_NET_RAW` を落とし、全スレッドと後から作るスレッドの CapEff・CapPrm・CapInh・CapBnd から消え、他は残ることを確かめる）を aofish の WSL の root で実行して通過。veil を root・`sandbox_keep_capabilities = ["CAP_NET_BIND_SERVICE"]` で起動し、全 10 スレッドの CapEff・CapPrm・CapBnd が `0x400` であることを確認。
- 文書: `docs/guide/routing.md`（英日。`sandbox_drop_capabilities` の行も追加）、リリースノート。

## 関連

- [B-219](B-219-seccomp-landlock-worker-threads-only.md)、[B-230](B-230-landlock-handled-access-and-add-rule-errors.md)、
  [F-282](../features/F-282-security-review-followup-hardening.md)
