# F-46: executor の Box&lt;dyn Future&gt; 排除・OP_TABLE スラブ化（F-34 / F-37 残件）

- **優先度**: P3
- **対応状況**: 未着手
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

## 依存・リスク

- 汎用 `spawn(future)` API が失われ拡張性が落ちる（設計哲学上は許容）。
- detach ガード・generation 管理のバグは UAF に直結するため、テストを厚くする。
