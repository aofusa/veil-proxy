# F-34: コネクション状態の Slab / Arena アロケーション

## 出典

`docs/artifacts/architecture_analysis_v4.md` / `v5.md`（Slab アロケーションの実装）。

## 概要

接続ごとの状態構造体（`HttpHandler` 等）の確保を、起動時に確保した固定長 Slab/Arena から
インデックス（トークン）で払い出す方式に変更し、実行時の `malloc`/`free` を排除する。

## 現状の問題

ホットパスのバッファ（`src/pool.rs` の `SafeReadBuffer`）は最適化済みだが、数万同時接続時、
接続ごとの状態構造体の確保がメモリアロケータ（mimalloc）のボトルネックになり得る。

## 改修内容

1. `src/pool.rs` を拡張し、`max_concurrent_connections` 上限に基づく固定長配列
   （`Box<[MaybeUninit<HandlerState>]>` + free-list）をスレッドローカルに確保。
2. 接続受理時にスロットを払い出し（O(1)）、切断時に free-list へ返却（O(1)）。
   トークン（インデックス）で状態を参照する。
3. Slab 枯渇時はヒープへフォールバック（接続上限を超えるバースト対策）。

## 受け入れ条件

- [ ] 全 E2E 回帰なし。
- [ ] 接続確立/切断のホットパスで状態構造体の `Box::new`/`malloc` が発生しない（Slab ヒット時）。

## 依存・リスク

- thread-per-core 設計のためスレッドローカル Slab が自然。`unsafe`（`MaybeUninit`）の不変条件を
  コメントで明示する必要がある。難易度高。

## 対応状況: HTTP/2 コネクションバッファのプール化を実装

接続ごとの最大の確保は **HTTP/2 コネクションの 64KB 読み込みバッファ**（`Http2Connection`
の `read_buf = vec![0u8; 65536]`）だった。これをスレッドローカルなフリーリストで再利用する。

- `src/http2/connection.rs`: `H2_READ_BUF_POOL`（thread-local `Vec<Vec<u8>>`）+ `acquire_h2_read_buf`
  / `release_h2_read_buf` を追加。`new_with_initial_buffer` はプールから取得し、`Drop for
  Http2Connection` で接続終了時に返却（再利用）。これで**接続ごとの 64KB malloc/free を排除**。
  thread-per-core のためロック不要。プールは最大 256 本、1MB 超の肥大バッファは戻さず解放。
- `read_more` 実行中（バッファ take 中）に drop された場合は read_buf が空のためプールに戻らず
  リークもしない。

### 検証

- `cargo test --bins --features full` 577 通過。
- E2E（features full）388/389（唯一の失敗 `test_error_handling_431` は負荷フレーキーで、単体
  実行では通過。HTTP/1.1 経路で本変更とは無関係）。segfault なし。

### 追記: HTTP/3 データプレーンの per-operation malloc 排除（F-33 と同時対応）

接続ごとの 64KB バッファ（HTTP/2）に続き、Slab 化の本質（**ホットパスでの実行時 malloc/free
排除**）を HTTP/3 の送受信ホットパスへ展開した（詳細は
[F-33-http3-gso-gro-offload.md](F-33-http3-gso-gro-offload.md)）。

- **受信**: メインループの 64KB 受信バッファを loop 外で一度だけ確保して再利用。
  データグラム毎の `vec![0u8; 65536]` + 2 回の `to_vec` を排除（GRO 受信と同時）。
- **送信**: `send_pending_packets` の送信スクラッチ（`send_buf` + GSO 連結バッファ + 境界）を
  **スレッドローカルのフリーリスト**で再利用（thread-per-core のためロック不要、`take`/`put`
  で `.await` 跨ぎ安全）。単一パケット送信の `to_vec` も `send_to_slice_async` で排除。

## 対応状況（全面書換）: executor のタスク管理をスラブ + index Waker へ刷新

接続ごとに発生していた **タスク確保（`Arc<Task>`）と 2 つのロック**を排除するため、
`src/runtime/executor.rs` のタスク管理を **全面書換**した。

### 旧実装の問題

- `spawn` ごとに `Arc::new(Task { future: Mutex<Pin<Box<dyn Future>>>, queue: Arc<Mutex<TaskQueue>> })`
  を確保（接続ごと: Arc 1 + Box 1 + 2 つの Mutex）。
- `wake`/`schedule` のたびに `Arc<Task>` をクローン（参照カウントの atomic 操作）し
  `Arc<Mutex<TaskQueue>>` をロックして push。

これらは `std::task::Waker` の `Send + Sync` 契約を満たすためだけの過剰同期だった。本ランタイムは
**すべての wake が所有ワーカースレッド上で発生する**（io_uring CQE は同スレッドで wake、ブロッキング
オフロード `offload.rs` も eventfd + POLL_ADD で起点スレッドへ通知し同スレッドで wake、Waker を
クロススレッドで呼ばない）。

### 新実装（thread-per-core スラブ executor）

- **スラブ**: スレッドローカルの `Vec<TaskSlot>` + free-list（`Vec<usize>`）。`spawn` は free-list から
  スロットを払い出し（O(1)、ヒープ確保なし）、完了でスロットを返却。`Arc<Task>` 確保を排除。
- **index ベース同一スレッド Waker**: Waker の data に `(slot index << 32 | generation)` をパックし、
  参照カウントを持たない（`clone` はビットコピー、`drop` は no-op）。`wake` はスレッドローカルの
  ready キュー（`VecDeque<(usize, u32)>`）へ index を push するだけ。**per-wake の Arc クローンと
  ロックを排除**。
- **世代カウンタ**: スロット再利用時に +1。解放済みスロットを指す古い Waker からの `wake` を弾き
  （`stale_waker_is_ignored` テストで検証）、誤った割り込みを防ぐ。
- **ロック撤廃**: `Mutex<future>` / `Arc<Mutex<TaskQueue>>` を `RefCell<ExecutorState>` 1 本に置換。
  単一スレッドのため十分。`block_on` の `Arc<AtomicBool>`+`Arc<Mutex<Option<R>>>` も `Rc<RefCell<_>>`
  に置換。
- **borrow 規律**: `run_ready_tasks` は poll 前に future をスロットから `take` し、**poll 中は
  `EXEC_STATE` を borrow しない**。これにより future 内からの `spawn`/自己 `wake`（いずれも
  `EXEC_STATE` を再 borrow）が再入安全。
- 公開 API（`spawn`/`Executor`/`block_on`/`current_executor`/`init_executor`）は維持。`Executor` は
  状態がスレッドローカルになったため ZST ハンドルへ。
- 残る確保は型消去のための `Box<dyn Future>` 1 本（spawn ごと、不可避）のみ。

### 検証

- 新規 executor 単体テスト 5 件（即時完了 / 自己 wake yield / 子タスク spawn & join / スロット再利用 /
  古い Waker の棄却）すべて通過。
- `cargo test --bins --features full` **585 通過**（既存フレーキー `cache::revalidation::test_active_count`
  はグローバル状態の並行干渉で本変更と無関係、単体実行で通過）。
- `cargo test --test integration_tests --features full` **53/53 通過**（実 io_uring の wake パスを
  TLS/HTTP/プロキシ負荷で検証）。
- E2E（features full）HTTP/3 **30/30 通過**。segfault なし。

### 追記: HTTP/3 データプレーンの per-operation malloc 排除（F-33 と同時対応）

Slab 化の本質（**ホットパスでの実行時 malloc/free 排除**）を HTTP/3 の送受信ホットパスへも展開
（詳細は [F-33-http3-gso-gro-offload.md](F-33-http3-gso-gro-offload.md)）。受信 64KB バッファの
loop 外再利用、送信スクラッチのスレッドローカル再利用、単一パケット `to_vec` 排除。

### 残（任意）

接続ごとの L7 状態構造体（`HttpHandler` 等）自体の Slab 化、および `Box<dyn Future>`（型消去）の
排除（型付きタスクプール）は、効果が限定的かつ汎用 spawn API と相反するため任意の継続課題とする。
本対応で **接続ごとの最大確保（64KB）と タスク確保（Arc）・ホットパスのロックを排除済み**。
