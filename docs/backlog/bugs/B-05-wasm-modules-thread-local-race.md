# B-05: WASM レスポンスモジュールのスレッドローカル競合

## 事象（再現手順）

1. WASM フィルタを有効にしたルートに並行リクエストを 10 本送信する
2. 一部リクエストで `X-Veil-Processed: true` がレスポンスに付与されない
3. 成功率が 40〜60% 程度にとどまり、80% のしきい値を下回る

## 影響

- 並行リクエスト時に WASM フィルタが不定期に未適用になる
- `test_wasm_concurrent_execution` が失敗（2〜4/10 しか成功しない）
- 実運用での WASM フィルタ信頼性が低下する

## 調査メモ

プロキシは monoio（シングルスレッド協調スケジューリング）で動作する。
各コネクションは別々の async タスクとして動く。
`WASM_RESPONSE_MODULES` を `thread_local! { static WASM_RESPONSE_MODULES }` に保存し、
リクエスト処理フェーズで `set_wasm_response_modules()` を呼び、
レスポンス処理フェーズで `get_wasm_response_modules()` を読む設計だった。

問題：リクエスト A がバックエンド I/O で yield すると、リクエスト B のタスクが割り込み
`set_wasm_response_modules([])` または別のモジュールリストで上書きする。
A が再開したとき `get_wasm_response_modules()` が B の（空の）値を返すため
WASM フィルタが適用されない。

```
Task A: set_wasm_response_modules(["header_filter"])
Task A: await backend_stream.read()  ← yield
Task B: set_wasm_response_modules([])  ← 上書き！（B は WASM なしのルート）
Task A: resume
Task A: get_wasm_response_modules()  → [] ← 誤り
```

## 改修案・対応内容

`WASM_RESPONSE_MODULES` thread_local ではなく、モジュールリストを関数引数として
呼び出しチェーン全体を通じて伝播させるよう修正した。

対象関数：
- `handle_backend`
- `handle_proxy`
- `transfer_response_with_compression`
- `proxy_https_request_with_compression`
- `transfer_https_response_with_compression`
- `handle_sendfile`

これにより各リクエストの async タスクが独立したモジュールリストを保持し、
他タスクの影響を受けなくなった。

修正後：並行 10 リクエスト全て（10/10）が正常に WASM フィルタ適用済み。

## 完了日

2026-06-20（本セッション対応済）
