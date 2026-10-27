# B-246: `seccomp_mode = "strict"` が違反したスレッドだけを消し、プロセスは動き続ける（文書は「プロセスを終了」）

**状態: 修正済み（v0.10.0）**

## 出典

B-244 の調査（2026-10-27）。`strict` の veil に SIGSTOP/SIGCONT を送ると、panic の出力も無く sleep していた背景スレッドが消え、
応答は返り続けるのに SIGTERM で終了しなくなった（SIGKILL が要った）。

## 事象

`src/security.rs` の `build_seccomp_filter` は `SeccompMode::Strict` の拒否動作を `0x00000000` とし、コメントに
`SECCOMP_RET_KILL_PROCESS` と書いていた。`0` は `SECCOMP_RET_KILL_THREAD`（`SECCOMP_RET_KILL_PROCESS` は `0x80000000`）。
違反したスレッドだけが SIGSYS で消え、残りのスレッドは動き続ける。

- 背景スレッド（リロード・ヘルスチェック・ディスク監視など）が消えても気づけず、終了処理がそのスレッドを待って止まる。
- ワーカーが消えると、そのワーカーの接続は応答されないまま残る。
- `examples/config.toml` は「違反したプロセスを SIGKILL」、利用者は「プロセスごと落ちて再起動される」と期待する。

既存の `test_seccomp_denies_disallowed_syscall` は単一スレッドの子で違反させるので、KILL_THREAD でもプロセスが終わり、区別できなかった。

## 修正（v0.10.0）

- `Strict` の動作を `0x80000000`（`SECCOMP_RET_KILL_PROCESS`、Linux 4.14+）にした。4.14 未満のカーネルは未知の動作の上位ビットを見ないので
  従来の KILL_THREAD になる。
- `test_seccomp_strict_kills_whole_process_from_other_thread`: 子で strict を掛け、別スレッドで禁止 syscall を呼ぶ。プロセス全体が SIGSYS で
  終わることを確かめる（修正前の値では主スレッドが `_exit(3)` まで進んで失敗する）。
- `docs/guide/security.md`（英日）にモードごとの動作を書き、`examples/config.toml` の説明を直した。

## 受け入れ条件

- 上の単体テストが通る。`VEIL_E2E_SECCOMP=strict` の E2E が通る（KILL_THREAD で隠れていた違反があればプロセスごと落ちて見える）。
