# B-242: `io_uring_enter` の一時的なエラー（EINTR・EBUSY・EAGAIN）を提出の経路で再試行せず、要求の失敗・ワーカーの停止・起動の失敗にする

**状態: 修正済み（v0.10.0）**

## 出典

2026-10-27 に aofish の Docker Desktop で回した `tools/container_security/chaos/syscall_chaos.sh`（strace の fault injection、`when=40+5`）。
`io_uring_enter:error=EBUSY`・`ENOMEM`・`EINTR`・`io_uring_setup:error=EFAULT` の 4 つすべてで veil が **起動の途中で exit 1**
（`docs/artifacts/security/scans/aofish-chaos-ext/syscall_chaos_report.txt`、git 管理外）。ハーネスは `exited` を「graceful に終了」として
findings に数えないので、集計は ok になる。

## 事象（コードの読解）

- `runtime/uring/ring.rs::enter` は `ret < 0` をそのまま `io::Error` で返す。`executor::submit_sqes`（`ring.submit()`）は `?` で各 Future
  （`ReadFuture`・`WriteFuture`・`SpliceFuture`・`SendBatch` …）へ返し、Future はその要求をエラーにする。
- `executor::wait_for_completions`（park）は `submit_and_wait(1)` のエラーのうち `Interrupted`（EINTR）だけ無視し、それ以外は
  `ftlog::error!("io_uring wait error")` で **`block_on` のループを抜ける**（`executor.rs` L1238 付近）。ワーカースレッドの `block_on` が
  戻ると、そのワーカーは接続を受けなくなる。
- カーネルが返し得る一時的なエラー:
  - `EBUSY`: `IORING_FEAT_NODROP` で CQ が溢れて overflow の一覧を流し込めないとき（CQE を消費してから再試行すれば通る。RT-07 の
    「CQ overflow を監視していない」と同根）。
  - `EAGAIN`: 非同期ワーカーの生成に失敗（`ENOMEM` 相当。待って再試行）。
  - `EINTR`: シグナルの配送（SIGHUP のリロード・SIGTERM）。`GETEVENTS` の待ちでしか返らないので park の無視で足りているが、
    `submit_and_wait` を使う他の経路（`executor.rs` L842 以外にあれば）も同じ扱いが要る。
- 起動時は main スレッドが同じランタイムで設定・証明書・DNS を処理するので、1 回の一時エラーが起動の失敗（exit 1）になる。
  `syscall_chaos.sh` の `when=40+5`（起動時の ring 初期化を避けるための開始オフセット）は、F-237 の io_uring 可用性プローブと
  起動時の I/O で消費されてしまい、**運転中の注入を試験できていない**。

## 改修案

- `submit`/`submit_and_wait` に有界の再試行を入れる: `EINTR` は即再試行、`EBUSY` は `consume_cqes`（`poll_completions`）してから再試行、
  `EAGAIN` は短い待ち（`sched_yield` 相当）で数回。上限を超えたら今までどおりエラー。ホットパスなので正常系に分岐を増やさない
  （エラーのときだけ入る `#[cold]` の関数へ）。
- park の `block_on` のループは `EBUSY`/`EAGAIN` でワーカーを止めず、CQE を消費して続ける。
- `syscall_chaos.sh` の開始オフセットを「起動後の最初の要求」に合わせる（起動の完了をヘルスチェックで待ってから strace を `-p` で attach する）。

## 受け入れ条件

- `syscall_chaos.sh` で、注入の最中に要求の失敗が一時的に増えても veil が `runtime=ok recovered=yes` になる（4 種とも）。
- `fuzz/fuzz_targets/io_uring_executor.rs` のモデルに enter の一時エラーを足し、ワーカーが止まらないこと。

## 修正（v0.10.0）

- `executor::submit_sqes`: `io_uring_enter` の一時的なエラー（EBUSY・EAGAIN・ENOMEM・EINTR）は SQE を 1 つも消費せずに返る（一部でも提出すれば件数が返る）ので、「まだ提出していない」として成功を返す（SQE は SQ に残り、次の park の `submit_and_wait` が提出する）。各 Future の要求を失敗させない。
- `executor::wait_for_completions`（park）: 一時的なエラーではワーカーを止めず、続く `poll_completions` で CQE を消費して（EBUSY の overflow を流し込める空きを作って）ループを続ける。EAGAIN・ENOMEM は `yield_now` で譲る。どちらも `#[cold]` の関数で、正常系の分岐は増やしていない。
- `block_on`: それ以外のエラー（EBADF 等）は以前どおり続けられないが、ループを抜けて結果の取り出しの `expect("future completed but no result")` で panic していたのを、原因の分かる `panic!("io_uring wait failed: ..")` にした。
- 試験: `ring::inject_enter_errors`（`cfg(test)` だけ。`enter` を指定の errno で指定の回数失敗させる）で、`executor_tests::b242_park_survives_transient_enter_errors`（タイマーの待ち。修正前は panic で試験のプロセスが abort）・`tcp::tests::test_b242_submit_survives_transient_enter_errors`（connect と書き込みの提出）。`io_uring_executor` の fuzz のモデルは `enter` を通らない（op テーブルと擬似 CQE だけ）ので、enter の失敗は実リングへの注入の単体試験で見る。
- `syscall_chaos.sh`: veil を普通に起動して要求に応答してから `strace -f -p 1` を attach して注入する（起動から strace で包むと起動の I/O がオフセットを使い切っていた）。開始オフセットの既定は 1。strace のイメージに `nonroot` 利用者を足した（コンテナの設定は `nonroot` へ特権を落とす。Debian には無い）。止まった（`exited`）・起動・attach の失敗も findings にする。

## 関連

- [B-24](B-24-sq-full-future-hang.md)（SQ 満杯）、[F-280](../features/F-280-security-review-info-hardening.md)（CQ overflow の監視）、
  [B-161](B-161-startup-failure-exit-zero.md)（起動の失敗の終了コード）
