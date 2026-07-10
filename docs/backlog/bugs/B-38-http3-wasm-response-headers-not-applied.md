# B-38: HTTP/3 経路で WASM レスポンスヘッダ変更が適用されない

## 事象

E2E `test_http3_wasm_integration`（F-91）が失敗する。

- 経路: HTTP/3 GET `/wasm/`
- 期待: `header_filter.wasm` が付与する `X-Veil-Processed` / `X-Veil-Filter-Version` / `X-Veil-Context-Id` がレスポンスに含まれる
- 実際: HTTP 200 だが、レスポンスヘッダはバックエンド由来のみ（`content-type`, `x-server-id` 等）。WASM ヘッダなし

### 再現手順

1. `./tests/e2e_setup.sh start`（default 設定で WASM `/wasm/*` 有効）
2. `cargo test --test e2e_tests --features full test_http3_wasm_integration -- --exact --nocapture`
3. 失敗メッセージに WASM ヘッダ無しのヘッダ一覧が出る

### ログ証拠

プロキシログにリクエスト側 WASM は動作:

```
[wasm:header_filter] Added request headers for context …
```

レスポンスヘッダはクライアントに反映されない。

## 影響

- HTTP/3 経由の Proxy-Wasm フィルタが **レスポンス改変として機能しない**
- 認証ヘッダ注入・レスポンスマスキング等が H3 のみ無効になるセキュリティ/機能ギャップ

## 調査メモ

`src/http3_server.rs` の `handle_request` 内 WASM 適用箇所:

```text
crate::wasm::FilterResult::Continue { .. } => {
    // ヘッダー変更はHTTP/3では複雑なため、現時点ではスキップ
    // 将来的に実装可能
}
```

リクエストヘッダ変更はログ上動くが、レスポンスヘッダの `on_response_headers` 適用が HTTP/3 応答組み立てに未配線の可能性が高い（HTTP/1.1 では E2E 通過）。

## 改修案

1. HTTP/3 バッファ経路で WASM `on_request_headers` の Continue 結果ヘッダを上流リクエストへ反映
2. 上流レスポンス受信後に `on_response_headers` を実行し、`send_response` のヘッダ一覧へマージ
3. ストリーミング経路（`http3_stream`）でも同等のフックを検討（WASM 付きルートは現状 Buffer 判定）
4. `test_http3_wasm_integration` を回帰固定

## 関連

- F-91 / `docs/artifacts/test_coverage_report.md` E-H3-12
- 類似: B-04（HTTPS パスの WASM）、B-30（HTTP/2 File の WASM）

## 対応状況

**完了**（2026-07-10）

- `src/http3_server.rs`: `on_response_headers` を `handle_proxy` 内で適用（`apply_h3_wasm_response_headers`）
- リクエスト側 `Continue` のヘッダ変更を上流へ反映
- 回帰: `test_http3_wasm_integration` PASS
