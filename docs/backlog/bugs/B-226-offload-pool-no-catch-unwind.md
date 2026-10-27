# B-226: FS の offload プールに `catch_unwind` が無く、閉包が 1 回 panic するとスレッドが減り、全滅すると以後の offload が永久に待つ

**状態: 修正済み（v0.10.0）**

## 出典

2026-10-27 のセキュリティレビュー（v0.9.2 の `src/runtime/offload.rs` の読解）。

## 事象

- `OffloadPool::spawn` のスレッドのループは `job()` をそのまま呼ぶ。panic するとそのスレッドは永久に消え、`workers` 回で全滅する。
- `offload()` は `slot`/`done` の `Arc` を持ち、`done` が立つまで `wait_readable_fd(eventfd)` を繰り返す。panic で `done` が立たないと、
  その要求は永久に `Pending`。全滅後は以後の全 offload（静的配信・DNS の解決・ディスクキャッシュ）がハングする。
- 現状の閉包は `std::fs` 系で panic しにくいが、`offload_handler`（ハンドラ用プール）は `Completer` の Drop で待機側を起こしていて、
  FS のプールだけ構造的な防御が無い。

## 改修案

- FS のプールにも `catch_unwind` と、結果スロットの Drop で `done` を立てて通知する仕組み（`offload_handler` と同じ）を入れる。
  panic した要求は `io::Error`（`Other`）として返す。
- スレッドが死んだら補充する（`spawn_pooled_with_panic_catch` の考え方。`src/system.rs:450`）。

## 受け入れ条件

- 単体: panic する閉包を `offload()` に投げ、呼び出し側がエラーで戻り、後続の `offload()` が完了すること。

## 修正（v0.10.0）

- FS の offload プールで閉包を `catch_unwind` で包み、panic でも結果スロットに `Err(payload)` を入れて `done` を立てる。
  起点のタスクは `resume_unwind` で panic を受け取り、接続ごとの `CatchUnwindFuture` がその要求だけを失敗させる（永久に待たない）。
- プールのスレッドのループでも閉包を `catch_unwind` で包む（スレッドを失わない）。キューの Mutex の poison も無視する。
  スレッドが死なないので補充は要らない。
- 単体: `b226_tests::panicking_job_does_not_kill_pool_or_hang`（ワーカー数より多く panic させても次の offload が返る）。

## 関連

- [F-29](../features/F-29-lockfree-cache-and-async-fs.md)（offload の導入）、[F-188](../features/F-188-api-library-core.md)（`offload_handler`）
