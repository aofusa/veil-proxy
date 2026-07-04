# F-61: WASM ボディフィルタ経路のアロケーション削減（F-43 残件）

## 出典

`docs/artifacts/completed_tasks_remaining_work_report.md`（F-43 残タスク）。

## 概要

WASM ボディフィルタ経路の copy（アロケーション）を排除する。ヘッダ・文字列・モジュール
リストのアロケーションは F-43 で排除済みだが、ボディは wasmtime ホスト関数へ所有データで
渡す制約上コピーが残っている。

## 現状

- Proxy-Wasm の async ABI（wasmtime へ渡す所有データ）の制約で、ボディを `to_vec()` して
  渡している。HTTP/1.1・HTTP/2・HTTPS・H2C の全フィルタ経路に波及する。

## 改修内容（案）

- エンジン API を借用ベース or `Arc<[u8]>` ベースへ再設計し、ボディの deep copy を排除。
- 全プロトコルのフィルタ適用経路を同時に更新。

## 受け入れ条件

- [ ] WASM ボディフィルタ経路に `to_vec()` / ボディ deep copy が無い。
- [ ] `/wasm/*` の E2E が全プロトコルで回帰なし。

## 依存・リスク

- wasmtime host function ABI の所有権制約。大きめの再設計。P3・独立フェーズ。

## 対応状況: 未着手