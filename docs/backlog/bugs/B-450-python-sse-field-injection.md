# B-450: Python の `EventSourceResponse` が単独の CR を行の終わりとして扱わず、event・id・retry の改行もそのまま書き、SSE のフィールド・イベントを差し込めた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「EventSourceResponse lets a lone CR in string data, or any newline in event/id/retry, inject SSE fields and whole
events」・「SSE event injection: EventSourceResponse does not neutralize CR/LF in event/id/retry, nor lone CR in data」
から確認。

## 事象

- `veil._sse_event` は文字列の `data` を `\n` だけで行に分けていた。SSE（WHATWG HTML の event-stream）は単独の `\r` も
  行の終わりとして解釈するので、利用者の文字列を中継するアプリ（チャット・通知・LLM の出力）では、`\r` を含む文字列で
  別のフィールド（`event:`・`id:`・`retry:`）やイベントを他の購読者のストリームへ差し込めた（`retry` で再接続を 1 日
  遅らせる、`id` で Last-Event-ID を変える等）。
- `event`・`id`・`retry` の値は改行の扱いなしにそのまま書いていた。

## 対応

- `data` は CRLF・LF・CR のどれでも行に分ける（sse-starlette と同じ）。
- `event`・`id` に CR・LF（`id` は NUL も）があれば、`retry` が負でない整数（数字だけの文字列を含む）でなければ
  `ValueError`（アプリの誤りとしてストリームを終える）。
- 利用者ガイド（`library.md` の英日）と docstring に書いた。

## テスト

- `crates/veil-python/tests/test_binding.py` の `TestServerSentEvents.test_line_breaks_cannot_inject_fields`。
