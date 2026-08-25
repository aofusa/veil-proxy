# F-161: HTTP/3 バックエンド起床通知のコアレッシング

**優先度**: P3
**ステータス**: 完了（16 ラウンドの交互 A/B で **+0.15%＝ノイズ内・非退行**。無駄な per-chunk 作業の削減として採用）
**関連**: F-151（イベント駆動メインループ・`ConnWaker`）、B-12（ダーティ集合の不変条件）

---

## 背景

`ConnWaker::notify()` は**レスポンスボディのチャンクごと**に呼ばれ、そのたびに

```rust
self.wake_queue.borrow_mut().push_back(self.cid.clone()); // Rc clone + VecDeque push
self.notify.notify();
```

を実行していた。メインループの `drain_wake_queue` は積まれた回数だけ `conns` の
HashMap ルックアップ（可変長バイト列のハッシュ）を行うが、`Http3Handler::dirty` により
2 回目以降のダーティ登録は何もしない＝**重複エントリは完全な無駄**だった。

## 改修内容

接続ごとに共有する `queued: Rc<Cell<bool>>` を `ConnWaker` に持たせ、メインループが
drain するまでは 2 回目以降の push を省く。`WakeQueue` の要素型は
`(ConnKey, Rc<Cell<bool>>)` になり、drain は **pop → `flag.set(false)` → `mark_dirty`** の
順で処理する。

**不変条件**: `queued == true` の間、その cid は `wake_queue` に 1 個だけ存在する。
flag を `mark_dirty` の**前**に false へ戻すため、`mark_dirty` 実行中に発生した通知は
新たに push され、次のイテレーションで確実に処理される（取りこぼしなし）。

## 計測（交互 A/B、h3_proxy 54,576B、16 ラウンド）

base 中央値 1,609.0 rps → new 1,611.4 rps = **+0.15%**、new 勝ち 8/16、標準偏差 1.5〜2.3%。
**差は完全にノイズ内**。1,600 rps 台では QUIC の暗号化・パケット化が支配的で、
チャンクごとの `Rc::clone` + `push_back` + HashMap ルックアップ削減は測定にかからない。
退行しないこと（採否基準）を確認したうえで採用した。

副産物として **HTTP/3 用の交互 A/B ハーネス `tools/perf/h3_ab.sh`** を追加した
（`run_perf.sh` は 1 イメージしか測れず改修前後の交互比較ができないため）。
