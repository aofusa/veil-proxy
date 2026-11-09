# B-354: io_uring で `offload` の待機者が通知を取りこぼし、負荷の末尾で要求が永久に止まる

**状態: 修正済み（feat/v0120-perf、2026-11-09）**

v0.12.0 の最終の計測（全項目の交互 A/B）が `h2_1_feat_compression` で 13 時間止まって見つけた。F-300（`c7374c7e`）の退行で、Linux の既定（io_uring）だけ。
**リリース前に直す必要のある不具合。**

## 事象

`veil:v0120-uring`（`c9689c23`）に `h2load -t4 -n 30000 -c100 -m10 -H 'accept-encoding: gzip, br, zstd'` をかけると、90% 付近で進まなくなる。

- 止まっている接続は 4 本（`ss` の Recv-Q・Send-Q は空。サーバもクライアントも待っているだけ）。
- ルートは File + 圧縮で `open_file_cache` も本体キャッシュも無い（要求ごとに 1 回 offload する）。
- 動いているプロセスの `/proc/1/fdinfo`: 32 本のリングのうち 2 本だけに POLL_ADD の未完了が残っている（5 本と 13 本）。
- 別のコンテナから接続数を多くした h2load を流す（全ワーカーに offload が届く）と、止まっていた h2load が完了した（`request max 47336.55s`）。

定常の負荷では次の offload の完了が拾うので見えない（遅延の裾にだけ乗る）。負荷の末尾など、次の完了が来ないときに永久に止まる。

## 原因

ワーカーごとの `offload` の待機者は 1 本の eventfd を共有する。待機のループは「`done` を確認 → POLL_ADD で待つ → カウンタを読み捨てる」。

F-300 で SQE の提出を park の `submit_and_wait` にまとめたので、POLL_ADD は他のタスクが動く間 SQ に残る。次の順で、シグナルが消える。

1. 待機者 X が `done` = false を見て POLL_ADD を積む（SQ にある。未提出）
2. X のジョブが別スレッドで終わる（`done` = true、eventfd へ write）
3. 既に起きていた待機者 A がカウンタを読み捨てる
4. park で X の POLL_ADD が提出されるが、カウンタは 0 なので眠る。次の offload の完了まで起きない

カーネルへ渡し済みの POLL_ADD は write の時点で個別に完了が記録されるので、読み捨てで消えない。F-300 の前は積むたびに提出していたので、
この窓が無かった。

## 対応

読み捨ての前に SQ を全部カーネルへ渡す（`executor::submit_all_queued_sqes`）。一時的なエラーで渡せなかったら読み捨てない（fd は読めるまま残るので、
次の POLL_ADD がすぐ完了する）。

- 読み捨ての前に積まれていた POLL_ADD は、提出の時点の readable で完了する。
- 読み捨ての後に `done` を確認する待機者は、完了の write が読み捨てより前なので `done` を観測する。

`wait_until_done` が複数の待機者が読み捨てる唯一の経路（F-299 の `Inbox` は受け手が 1 タスク、reactor は `wake_all_readers`）。増える syscall は、SQ に残りが
あるときの `io_uring_enter` 1 本だけで、offload のスレッドの往復に比べて無視できる（[計測](../../perf/platform_verification.md)）。

## テスト

- 単体（io_uring）: `runtime::offload::drain_tests::a_drain_keeps_the_signal_of_a_wait_still_in_the_sq`（上の順を確定させる）、
  `many_concurrent_offloads_all_complete`（64 タスク × 64 回）。
- 全項目の交互 A/B が止まらずに完走すること。
