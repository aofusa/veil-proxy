# B-202: ASGI のストリーミング応答で、最後の断片の後にアプリのタスクを cancel して background が消える

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の Python・C バインディング・リリースのレビュー（docs/artifacts/v0100/review-bindings-release.md） #1（2026-10-26）。

## 事象

- `veil.asgi` のストリーミング経路（`chunks()`）は、最後の `http.response.body` を受け取ると `finally` で未完了のアプリの
  タスクを `cancel()` していた。Starlette はその後に `background` を await するため、`StreamingResponse(background=...)` と、
  `BaseHTTPMiddleware` のある FastAPI のすべての応答の `BackgroundTasks` が打ち切られた。

## 修正

- 応答を最後まで送れたときはタスクを cancel しない（クライアントの切断等で途中で閉じたときだけ cancel する）。
- 残ったタスクはモジュールの集合（`_RUNNING`）で参照を持ち、終わったら外す（GC で消えないように）。

## 試験

- `crates/veil-python/tests/test_fastapi_compat.py` の `test_streaming_response_keeps_background`・
  `test_streaming_response_background_with_middleware`。

## 実施内容

- `crates/veil-python/python/veil/asgi.py`。文書: `crates/veil-python/docs/fastapi.md`。
