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

## 実装（2026-07-05）

1. **Pause/resume のインライン配線**（`src/wasm/engine.rs`）:
   - ヘッダコールバックが `Pause` を返し pending HTTP コールが登録されている場合、
     エンジンが**同一インスタンス**でインライン解決する
     `resolve_pending_http_calls_inline` を実装:
     上流解決（`CURRENT_CONFIG.upstream_groups`、tick スレッドと同一規約）→
     ブロッキング HTTP クライアントを `runtime::offload` で専用スレッドへ退避
     （**イベントループはブロックしない**）→ 応答をコンテキストへ格納 →
     `proxy_on_http_call_response` を `call_async` で呼び resume。
   - resume 後にモジュールが追加コールを登録した場合は継続解決
     （`max_http_calls` 回で打ち切り）。ローカルレスポンス設定時は即時返却。
   - 解決済みコールはグローバルレジストリから除去し tick スレッドの二重実行を防止
     （`remove_global_pending_call` 新設）。
   - リクエスト/レスポンス両方のヘッダフェーズ（HTTP/1.1・HTTP/2 で共用の
     エンジン API）に適用。pending コールが無い純粋な Pause は従来どおり。
2. **`http_call_filter.wasm`**（`examples/wasm-filters/http-call-filter/`）:
   dispatch_http_call → Pause → on_http_call_response でコール結果ステータスを
   含むローカルレスポンスを返すテスト/ベンチ用フィルタ。
   `tests/wasm/http_call_filter.wasm` としてビルド済み配置、E2E 設定
   （`/wasm-http-call/*` ルート + `bad-pool` 上流）へ配線。
3. **ベンチマーク**: `benches/wasm.rs` に `wasm_http_call` グループを追加
   （header_filter との比較で Pause/resume + 上流コール 1 回のオーバーヘッドを計測）。
4. **付随修正**: HTTP/1.1 ローカルレスポンスの `Content-Length` 自動補完
   （欠落時にクライアントがハングしていた）、`http_executor` の応答ヘッダへ
   `:status` 擬似ヘッダ付与（SDK の `get_http_call_response_headers` 互換）。
5. **検出バグ**: 実装中に [B-19](../bugs/B-19-proxy-wasm-abi-mismatch.md)
   （Proxy-Wasm ABI 不一致）と [B-20](../bugs/B-20-wasm-sync-call-async-store-panic.md)
   （async store での同期 call panic）を検出し修正。

テスト: E2E `test_f62_wasm_http_call_pause_resume` / `test_f62_wasm_http_call_concurrent_requests`
（並行 8 リクエスト）、既存 wasm 単体 + E2E 全 417 件回帰なし。

## 対応状況: 完了（2026-07-05）