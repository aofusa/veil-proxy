# F-166: gRPC 中継と epoll reactor の最適化（`full-container`）

## 背景

2026-08-25 のフルスイート（`docs/perf/README.md`）で `full-container`（epoll reactor）は
io_uring 既定ビルドと全 122 ペアの比の中央値 1.009＝互角。ただし
**gRPC 中継は 4,565 rps（k6）** に留まり、reactor 固有の syscall 固定費と
gRPC 中継経路のアロケーションが残っている。

## A. epoll reactor の syscall 削減

現状、1 回の I/O 待機（`Readable`/`Writable` の `Pending` 経路）ごとに

1. 確認用 `poll(2)` 1 発（`reactor/tcp/unix.rs::Readable::poll`。kqueue は F-141 の
   readiness ヒントで省略済みだが **epoll には無い**）
2. `epoll_ctl(MOD)` 1 発（`EPOLLONESHOT` の再武装。`executor::register` は
   「判定ミスの余地を消すため」無条件に発行している）

の 2 syscall を払っている。

**改修案（設計変更あり）:**

- **A-1: epoll も readiness ヒントを持つ。** `dispatch_event`（epoll 版）が `EPOLLIN`/`EPOLLOUT`
  を `FdRecord::read_hint`/`write_hint` へ立て、`Readable`/`Writable` が consume-once で
  読んで確認用 `poll(2)` を省く（kqueue と同じ仕組み・同じ不変条件）。
  **`poll(2)` フォールバックは削除しない**（AGENTS.md の禁止事項）。
- **A-2: `EPOLLONESHOT` + 毎回 MOD をやめ、`EPOLLET` で fd あたり 1 回だけ ADD する。**
  readiness はユーザ空間（`FdRecord`）で追跡し、`epoll_ctl` は
  「初回 ADD」と「close 時の暗黙 DEL」だけにする。ET のエッジ取りこぼしは
  A-1 の consume-once ヒント + `poll(2)` フォールバックで塞ぐ
  （ヒントが無い＝エッジを受けていない状態では必ず `poll(2)` で現状を確認してから park する）。
- **A-3: `FdRecord` の `Vec<Waker>` を「インライン 1 枠 + あふれ Vec」にする。**
  通常 1 fd 1 待機者なので、待機・起床のたびの malloc/free が丸ごと消える
  （共有 eventfd の複数待機はあふれ側で従来どおり動く）。
- **A-4: `park` の `epoll_wait` バッチと `fire_expired` の順序を uring 版（B-72）と揃える。**

## B. gRPC 中継のアロケーション削減

F-165 の A1〜A4・A6 がそのまま gRPC 中継（`h2_proxy_h2c` → `H2cClient`）の
1 RPC あたりのコストになる。加えて:

- **B-1: `H2C_POOL` のキーを `String` から借用キー検索へ**（`put` のたびに `to_string()` している）。
- **B-2: `H2cResponse` を `Bytes` ベースにする**（受信バッファのスライス共有）。
  下流へ渡す `H2RespMsg::Head` のヘッダ型も合わせ、`h2_proxy_h2c` の
  ディープコピー（`k.clone(), v.clone()`）を消す。
- **B-3: HPACK エンコード出力・フレームエンコード出力をプールバッファへ**。

## 検証

- `tools/perf/h2c_proxy_lab.sh strace` で 1 req あたり syscall 数（epoll: 改修前後）。
- `alloc-stats`（F-165）で 1 RPC あたりアロケーション数（gRPC: 改修前後）。
- 交互 A/B（`ab` サブコマンド）で rps。gRPC は `tools/perf/run_perf.sh` の
  `CONFIG_GLOB=grpc_h2_*` scoped 実行。
- **io_uring 既定ビルドの非退行も必ず確認する**（F-158 の教訓: 片方のバックエンドで
  効いた変更をもう一方へ一般化しない）。
