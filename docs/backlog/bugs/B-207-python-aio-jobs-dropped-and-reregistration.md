# B-207: Python の常駐ループ: 捨てられたジョブでワーカーが永久に待ち、登録が 1 回きりで 2 回目の run が失敗する

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の Python・C バインディング・リリースのレビュー（docs/artifacts/v0100/review-bindings-release.md） #6 #8（2026-10-26）。

## 事象

- 常駐ループ（`aio`）の待ち行列に積んだ呼び出し・WebSocket のジョブが、ループの停止やジョブの組み立て（`WebSocket` の構築）の
  例外で処理されずに捨てられると、ワーカー側は完了を永久に待った。
- 常駐ループの登録が `OnceLock` で、同じプロセスで `App.run` を 2 回呼ぶと 2 回目が失敗し、停止後も閉じた fd を持ち続けた。

## 修正

- 捨てられるジョブ（停止後の push・取り出し後の例外・未処理の drop）は、呼び出しなら 500 で閉じ、WebSocket なら 500 で
  拒否する（`settle`・`AioJob` の Drop）。
- 登録を `AtomicPtr` にし、`aio_teardown()`（停止時に `_aio.stop()` から呼ぶ）で外して待ち行列を閉じ、残りを処理する。
  次の `run` で登録し直せる。
- `_run_ws` は `WebSocket` の構築も try の中で行い、失敗したら拒否する。

## 試験

- `crates/veil-python/tests/test_aio.py` の `test_websocket_construction_failure_rejects`、
  `crates/veil-python/tests/test_native.py` の `TestAioRegistration.test_setup_teardown_setup`。

## 実施内容

- `crates/veil-python/src/aio.rs`・`src/lib.rs`、`python/veil/_aio.py`・`websockets.py`・`_veil.pyi`。
