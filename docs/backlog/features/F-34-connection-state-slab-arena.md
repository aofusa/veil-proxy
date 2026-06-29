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

### 残（より広範な Slab 化）

L7 状態構造体やタスク（executor の `Arc<Task>` + `Box<dyn Future>`）自体の Slab/Arena 化は、
ランタイムのタスク管理（Waker / Send+Sync 制約）の書き換えを要し全接続に影響するため、独立した
大規模タスクとして継続する。本対応は接続ごと最大確保（64KB バッファ）の排除を優先し、さらに
HTTP/3 送受信ホットパスの per-operation malloc を排除した。
