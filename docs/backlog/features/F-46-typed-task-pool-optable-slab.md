# F-46: executor の Box&lt;dyn Future&gt; 排除・OP_TABLE スラブ化（F-34 / F-37 残件）

- **優先度**: P3
- **対応状況**: 完了（2026-07-03）
- **出典**: `docs/artifacts/remaining_tasks_analysis.md` F-34 / F-37 残件 / アイデア3

## 機能説明・現状

- executor のタスクは F-34 でスラブ + index Waker 化済みだが、タスク本体は依然 `Box<dyn Future>`（動的ディスパッチ + 接続ごとヒープ確保）。
- OP_TABLE は F-37 で軽量ハッシュ + 事前確保済みだが、依然ハッシュマップ。`user_data` に slot index + generation をパックした純粋な配列インデックス（Slab）化が残る。

## 改修内容

- 接続ハンドラ種別（HTTP/1, HTTP/2, HTTP/3, L4, 内部タスク等）を enum で型付けしたタスクプールに移行し、`Box<dyn Future>` と spawn 時ヒープ確保を排除する。
- OP_TABLE を Slab 配列に置換し、`user_data = (generation << 32) | slot` 方式で detach ガードの意味論（キャンセル済み op の completion 無視）を維持する。

## 受け入れ条件

- spawn / op 発行のホットパスからハッシュルックアップと `Box::new` が消える。
- 全 E2E（features full）が通過し、ストレステストで UAF / 二重 poll が発生しない（B-07 回帰確認）。

## 調査メモ（2026-07-02）

- `runtime::spawn` の `Box<dyn Future>` 利用箇所と OP_TABLE（Fibonacci ハッシュ HashMap、F-37 済み）を確認。detach ガード（B-07 の UAF 対策）が user_data キーの意味論に依存しており、slot+generation パックへの移行は detach/cancel 経路の全面書き換えを伴う。
- UAF 直結領域のため、専用の設計レビューとストレステスト（B-07 回帰）を伴う独立フェーズとして継続する。

## 依存・リスク

- 汎用 `spawn(future)` API が失われ拡張性が落ちる（設計哲学上は許容）。
- detach ガード・generation 管理のバグは UAF に直結するため、テストを厚くする。

## 実装内容（2026-07-03 完了）

### OP_TABLE スラブ化

- Fibonacci ハッシュ `HashMap`（F-37）を **スロット配列 + free-list** に置換。
  `user_data = (世代 << 32) | index` のパックで、per-op のハッシュ計算・probe を排除
  （register/set_waker/take/peek/on_cqe すべて配列インデックス 1 回）。
- **detach ガードの意味論は世代カウンタが担保**（B-07 の UAF 対策）: スロット解放で
  世代 +1 し、stale な CQE / Waker / detach を `resolve()` の世代不一致で弾く。
- 世代は 1 始まり・0 スキップで、`user_data = 0` を無効 ID として予約
  （Future のフィールド初期値 0 が実 op を誤参照する事故を防ぐ）。
  `CANCEL_SENTINEL (u64::MAX)` は index = u32::MAX で自然に不一致。
- API 変更: `next_user_data()` + `register_op(ud)` → `alloc_op() -> u64`（確保 = 登録）。
- `OpGuard` を enum 化（`Noop` / `Cleanup(Box<dyn FnOnce>)`）し、後始末不要 op の
  detach ごとのクロージャ確保を排除。
- **付随バグ修正**: `Sleep`（TIMEOUT op）が in-flight のまま drop されると（`timeout()` で
  内側 Future が勝つたびに発生）テーブルエントリが永久残留するリークを発見・修正。
  `detach_op_no_cancel` + `Noop` ガードで満了 CQE 時にスロット解放する。

### 型付きタスクプール（Box&lt;dyn Future&gt; 排除）

- `runtime::TaskPool<F>` を新設: 同一の具象 Future 型 `F`（spawn 呼び出しサイトごとの
  `async` ブロック型）を **チャンク化スラブにインライン格納**。spawn の定常コストは
  スラブスロット再利用 + `Rc` クローンのみで、ウォームアップ後は malloc ゼロ。
- Pin 健全性: チャンクは `Box<[RefCell<Option<F>>]>`（ヒープ固定位置）で、チャンク列の
  成長でも要素は移動しない。poll は in-place（`Pin::new_unchecked`）。
  巨大 async Future（接続ハンドラは数百 KB）による**スタックオーバーフローを避けるため
  チャンクは Vec 経由でヒープ上に直接構築**（`Box::new([..; N])` はスタック経由で NG、
  E2E で実際に検出して修正）。チャンク粒度は 16 スロット。
- エグゼキュータのタスク本体を `TaskBody` enum（`Boxed` / `Pooled { Rc<dyn PoolPoll>, slot }`）
  に拡張。ディスパッチは従来の `dyn Future::poll` と同等（間接呼び出し 1 回）。
- 適用サイト: HTTPS 接続ハンドラ・H2C 接続ハンドラ・HTTP リダイレクト（main.rs）、
  L4 接続ハンドラ、**HTTP/3 バックエンドタスク（リクエストごと・最ホット）**。
  h3 は具象型を命名できないため `BackendSpawner`（プールを閉じ込めた `Rc<dyn Fn>`、
  スレッドごと 1 個）で配布。`CatchUnwindFuture` はジェネリック化して内部 Box も排除。

## 受け入れ条件の検証

- spawn / op 発行のホットパスからハッシュルックアップと `Box::new` が消える
  → OP_TABLE は配列インデックス化、接続/リクエストの全ホット spawn はプール化 ✓
- 全 E2E（features full）が通過し、ストレステストで UAF / 二重 poll が発生しない
  → **E2E 全 406 テスト通過**（並列）、`test_concurrent_connection_stress`（200 同時接続、
  B-07 回帰）通過、単体 644（OP_TABLE スラブ 6 + TaskPool 3 の新規テスト含む）・統合 53 通過 ✓
