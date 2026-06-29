# F-37: ランタイム最ホットパスの per-op 同期・ハッシュコスト排除（OP_TABLE / user_data）

## 出典

F-08/F-32/F-33/F-34 完了後の「ホットパスでの同期処理・メモリアロケーション・ロック調査」
（作業指示の事後調査フェーズ）。F-34（executor のタスク管理スラブ化）の姉妹最適化。

## 概要

io_uring の **全オペレーション**（recv/send/accept/connect/splice/timeout/poll_add）の
登録・Waker 設定・完了取り出しで必ず通る `src/runtime/executor.rs` の `OP_TABLE` と
`user_data` 採番は、プロキシで最も高頻度に実行される「最ホットパス」である。ここに残って
いた **per-op の暗号学的ハッシュ計算** と **グローバルアトミック競合（偽共有）** を排除した。

## 現状の問題（修正前）

1. **`OP_TABLE` の SipHash**: `OpTable.ops` / `OpTable.detached` は標準 `HashMap<u64, _>` で、
   既定の SipHash（暗号学的ハッシュ）を u64 キーに毎 op 適用していた。キーは内部の単調増加
   カウンタ由来で外部入力ではない（衝突攻撃の対象外）ため、SipHash は純粋な CPU 浪費。
   また初期容量 0 からの成長で early-growth の再確保（malloc）が発生していた。
2. **`user_data` のグローバルアトミック**: `next_user_data()` が **グローバル** `AtomicU64`
   を毎 op `fetch_add` していた。thread-per-core では **全ワーカーコアが同一キャッシュ
   ラインを奪い合う偽共有**となり、コア数に対するスケールを阻害する（同期コスト）。

## 改修内容

1. `OP_TABLE` 専用の超軽量ハッシャ `NoHashU64`（黄金比定数 1 回乗算の Fibonacci ハッシュ）を
   導入し、`ops`/`detached` を `HashMap<u64, _, BuildHasherDefault<NoHashU64>>` に変更。
   per-op の SipHash を排除（乗算 1 回のみ。SwissTable の制御バイト用に上位 bit も拡散）。
   さらに `with_capacity_and_hasher(256, …)` で事前確保し early-growth の再確保を抑制。
2. `USER_DATA_COUNTER` を **グローバル `AtomicU64` → スレッドローカル `Cell<u64>`** へ変更。
   user_data はリング・OP_TABLE がともにスレッドローカルで「提出スレッド＝完了処理スレッド」
   （offload も eventfd で起点スレッドへ戻る）のため、一意性はスレッド内で足りる。毎 op の
   アトミック競合を排除（別スレッドが同値を持っても別テーブル・別リングで衝突しない）。

いずれも **公開 API（`next_user_data`/`register_op`/`detach_op` 等）と UAF ガード意味論
（B-07）を一切変えない**ため、36 箇所の op 生成 Future の呼び出し側は無改修。

## 受け入れ条件

- [x] 単体（`--bins`）・統合（`integration_tests`）・E2E（`full`）が回帰なし。
- [x] io_uring op 登録/完了のホットパスに暗号学的ハッシュとグローバルアトミックが無い。
- [x] B-07 の UAF ガード（detach/cancel）意味論が不変。

## 依存・リスク

- user_data のスレッド間一意性に依存する箇所が無いことを確認済み（全 `next_user_data` 呼び
  出しは同一スレッドで register→complete、`offload.rs` は user_data を跨スレッドで渡さない）。
- ハッシャ誤実装は性能劣化のみ（正当性は不変）。`op_table_nohash_roundtrip` 単体テストで
  拡散とライフサイクルを検証。

## 対応状況: 完了

### 実装（`src/runtime/executor.rs`）

- `NoHashU64`（Fibonacci 乗算）＋ `OpMap<V>` 型エイリアス、`OP_TABLE_PREALLOC = 256`。
- `next_user_data()` をスレッドローカル `Cell<u64>` 採番に変更（アトミック撤廃）。
- 単体テスト `op_table_nohash_roundtrip` を追加（ハッシュ拡散＋register/on_cqe/take の往復）。

### 検証

- `cargo test --bins --features full` **587 通過**（executor 6 件、新規ハッシャテスト含む）。
- `cargo test --test integration_tests --features full` **53/53 通過**（実 io_uring の
  recv/send/accept/splice を TLS/HTTP/プロキシ負荷で検証）。
- E2E（features full）**391/391 通過**。segfault なし。

### 残（任意・大規模）

`OP_TABLE` 自体の **スラブ化**（user_data に slot index + generation をパックし HashMap を
配列インデックスへ）は、B-07 の detach ガード意味論を保ちつつ行う必要があり影響が大きい。
本対応で **per-op の SipHash と グローバルアトミック競合は排除済み**であり、HashMap は
steady-state で再確保しない（事前確保）ため、スラブ化は効果限定的かつ高リスクとして任意の
継続課題とする。
