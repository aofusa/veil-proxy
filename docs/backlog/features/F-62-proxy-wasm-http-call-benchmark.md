# F-62: Proxy-Wasm 「HTTP コールあり」フィルタのベンチマーク（F-48 残件）

## 出典

`docs/artifacts/completed_tasks_remaining_work_report.md`（F-48 残タスク）。

## 概要

外部への「HTTP コール」を行う WASM フィルタのベンチマークシナリオを追加する。

## 現状

- ホスト側 `dispatch_http_call` の `FilterResult::Pause` と後続 `resume`（再開処理）が
  プロキシのリクエスト経路に **未配線** のため、End-to-End で機能しない。
- そのためベンチマークは追加できない（F-48 では対象外と明記）。

## 改修内容（案）

1. ホスト実装: `dispatch_http_call` → `Pause` → 上流 HTTP コール → 応答受領 → `resume` の
   配線をリクエスト経路（HTTP/1.1・HTTP/2）に実装。
2. `benches/wasm.rs` に「HTTP コールあり」フィルタのシナリオを追加。

## 受け入れ条件

- [ ] Pause/resume が E2E で機能（HTTP コールする WASM フィルタが応答を返す）。
- [ ] ベンチにシナリオ追加。

## 依存・リスク

- Pause/resume 配線はリクエスト経路の状態機械に影響。P3・継続課題。

## 対応状況: 未着手