# B-288: Swagger UI のオフライン試験が Try it out を描画前に数え、`examples-nightly` が落ち続けていた

**状態: 修正済み（v0.10.1）**

## 出典

`examples-nightly` の `swagger-offline`（v0.10.0 の作業が入った dd1555e5 以降、毎晩失敗。例: run 41818396269）。

## 事象

`tests/api/swagger_offline.sh` の rust-websocket の検査が `FAIL Try it out: ['/plaintext: Try it out missing']` で失敗した。
`/ws`・`/typed`（WebSocket の操作）に Try it out が無いことは通っていた。

## 原因

試験の誤り（製品の不具合ではない）。Swagger UI は操作を開くと先に `.opblock-body` を描き、操作の解決（非同期）が済んでから
Parameters の見出しと Try it out のボタンを描く。`tests/api/swagger_offline.py` は `.opblock-body` が出た直後にボタンを数えていた。
最初に開いた操作では解決が間に合うことがあるが、`/ws`・`/typed` の後に開く `/plaintext` では間に合わず「無い」と判定した。
macOS の Chrome で同じ順に開くと、直後は 0 個・3 秒後は 1 個になることを確かめた（`allowTryItOutFor('/plaintext','get')` は `true`）。
WebSocket の操作の判定も同じ理由で、描かれる前に数えて偶然通っていた。

## 修正（v0.10.1）

- `tests/api/swagger_offline.py`: ボタンのある Parameters の見出し（`.opblock-section-header`）が描かれるのを待ってから数える
  （`ws:`・`try:` のどちらも、描画済みの状態で判定する）。

## 関連

- [F-264](../features/F-264-swagger-ui-offline.md)
