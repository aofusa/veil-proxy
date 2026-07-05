# B-16: kTLS splice パイプ取得の RefCell 二重借用でデータプレーンが panic

## 出典

`tools/container_security/chaos/bad_backend_chaos.sh`（F-67 で追加したバックエンド
プロトコル違反カオス）。不正なバックエンド応答を返すモックへ Veil をプロキシさせた際に
顕在化。**2026-07-05 修正済み**（下記「修正内容」参照）。

## 事象（再現手順）

1. `veil:glibc` を `ktls_enabled = true` / `threads = 0`（4 ワーカー）でプロキシ設定し、
   上流に「巨大ヘッダー / ヘッダー途中切断 / 不正ステータス / 即クローズ」を返す
   モックバックエンド（`bad_backend_server.py`）を配置する。
2. `/huge-headers` `/truncated-headers` `/bad-status` `/instant-close` などへ HTTPS で
   並行アクセスする（`bad_backend_chaos.sh` の 8 プローブ）。
3. Veil のワーカースレッドが panic する：

   ```
   thread '<unnamed>' (15) panicked at src/pool.rs:401:30:
   ERROR [src/system.rs:198] Task panicked during poll: Any { .. }
   ```

   `src/pool.rs:401` は `get_splice_pipe()` 内の `let mut pipe = p.borrow_mut();`。
   タスク poll ハンドラ（`system.rs:198`）が捕捉するためプロセスは継続するが、
   当該リクエスト/接続は異常終了する。

- **再現性**: 間欠的。4 ワーカー + 全 8 プローブ並行で再現、1 ワーカーでは未再現。
  並行度・スケジューリング依存の競合と推定。

## 影響

- **P0 候補**。データプレーンでの panic。AGENTS.md ホットパス絶対規則
  「`RefCell` 借用を await 跨ぎで保持しない」に反する典型。
- panic はタスク単位で捕捉されプロセスは落ちないが、
  上流異常を契機に外部からトリガ可能なため DoS 面のリスクがある。

## 調査メモ（未確定・修正前）

- `get_splice_pipe()`（`src/pool.rs:399` 付近）は `SPLICE_PIPE` スレッドローカルの
  遅延初期化のため `p.borrow_mut()` を行い、最終的に `Ref`（不変借用）を返す設計。
- 返却された `Ref` を **await を跨いで保持** したまま、同一スレッドの別経路（または
  同一フューチャーの再入）で再度 `get_splice_pipe()` → `borrow_mut()` が走ると
  `RefCell` が既に不変借用中で `BorrowMutError` → panic する、という二重借用が疑わしい。
- 不正バックエンド応答（早期切断・巨大ヘッダー）で splice 経路のエラーパスが
  通常と異なる順序で `get_splice_pipe()` を呼ぶことが引き金と推定。

## 修正内容（2026-07-05）

根本原因は 2 つ:

1. `get_splice_pipe()` が `RefCell` の `Ref` を `unsafe transmute` で `'static` 化して
   返却し、呼び出し側（`proxy_http_request_splice`）が **await を跨いで保持**していた。
   その間に同一スレッドの別タスクが `get_splice_pipe()` を呼ぶと、遅延初期化のための
   無条件 `borrow_mut()`（初期化済みでも実行される）が `BorrowMutError` で panic。
2. そもそも単一のスレッドローカルパイプを複数タスクが await 跨ぎで共有すると、
   splice 途中データの**混線**（別リクエストへの応答データ漏れ）リスクがあった。

修正（`src/pool.rs` / `src/proxy.rs`）:

- `SPLICE_PIPE`（`RefCell<Option<SplicePipe>>`）を L4 パイプツール（F-40）と同じ
  **checkout/return 型プール** `SPLICE_PIPE_POOL`（`RefCell<Vec<SplicePipe>>`、上限 64）へ置換。
- `get_splice_pipe()` は所有権ベースの RAII ガード `PooledSplicePipe` を返す。
  借用はプール pop/push の同期スコープ内でのみ発生し、await 跨ぎの `Ref` 保持が構造的に不可能。
- ガードの Drop で FIONREAD により**残データが無い場合のみ**プールへ返却
  （残データあり・ioctl 失敗・満杯時は破棄）。データ混線を防止。
- `proxy.rs` 側の `#[allow(unused_assignments)]` によるライフタイム延命ハックも撤去。

回帰テスト: `src/pool.rs::tests::splice_pipe_pool`（同時 2 ガード保持で panic しない /
クリーンなパイプの再利用 / 残データありパイプの破棄 / プール上限）。
`bad_backend_chaos.sh`（F-67）も回帰確認に使用可能。

## 関連

- テスト: [F-67](../features/F-67-backend-protocol-violation-tests.md)
- 規約: AGENTS.md「ホットパス絶対規則」
