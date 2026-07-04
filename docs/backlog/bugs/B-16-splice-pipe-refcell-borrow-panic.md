# B-16: kTLS splice パイプ取得の RefCell 二重借用でデータプレーンが panic

## 出典

`tools/container_security/chaos/bad_backend_chaos.sh`（F-67 で追加したバックエンド
プロトコル違反カオス）。不正なバックエンド応答を返すモックへ Veil をプロキシさせた際に
顕在化。**本チケットは事象記録のみ。修正は未実施。**

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

## 改修案（実施しない）

- `get_splice_pipe()` が `Ref` を返さず、必要な操作をクロージャ内で完結させる
  （`with(|pipe| ...)` パターン）か、借用を await 跨ぎで保持しない呼び出し規約に変更する。
- あるいは初期化を接続開始時の同期地点に前倒しし、ホットパスでの `borrow_mut()` を排除する。
- 修正時は `bad_backend_chaos.sh` を回帰テストとして用いる。

## 関連

- テスト: [F-67](../features/F-67-backend-protocol-violation-tests.md)
- 規約: AGENTS.md「ホットパス絶対規則」
