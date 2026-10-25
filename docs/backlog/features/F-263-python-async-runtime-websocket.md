# F-263: Python: 常駐の asyncio イベントループ・WebSocket・プロトコルアップグレード・lifespan

**状態: 未着手（2026-10-25 起票・設計。親は [F-258](F-258-v0100-plan.md)。前提は [F-261](F-261-handler-http11-upgrade.md)・[F-262](F-262-handler-websocket-rust.md)）**

## 背景

- `async def` のハンドラは、プールのスレッドごとのイベントループで **要求ごとに** `loop.run_until_complete(coro)` する
  （`crates/veil-python/python/veil/__init__.py` `_run_coro`）。コルーチンが待っている間もプールのスレッドを 1 本占有するので、
  同時に待てる要求の数 = `[api] handler_threads`。HttpArena の async（`/delay/{ms}` を 32K 同時）・WebSocket の 16,384 接続は捌けない。
  ループは要求の外で回らないので、要求をまたぐタスク（バックグラウンド・定期処理・接続の常駐）も置けない。
- `crates/veil-python/docs/websocket.md` は「Python のハンドラは WebSocket を終端できない」と書いている。
- FastAPI の利用者が期待する `@app.websocket`・`lifespan`・`on_event("startup")` が無い（[F-268](F-268-python-kit-fastapi-compat.md) の土台にもなる）。

## 制約

- ライブラリ利用時だけ（`crates/veil-python` と `src/api/` の `handler` の範囲）。本体のバイナリは変わらない。
- **ワーカー（I/O のスレッド）は GIL を取らない**。ワーカーとループの間は有界のキュー + 通知 fd（`runtime::offload::await_completion`
  と同じ。AGENTS.md「スレッド越しに Waker を呼ばない」）。
- 同期のハンドラ（`def`）の経路・`offload=False` の経路は変えない（`tools/api-bench` の Python の数値を落とさない）。

## 設計

### 1. 常駐のイベントループ（`veil._aio`、Rust 側は `crates/veil-python/src/aio.rs`）

- `veil.run()` の開始時に **ループ専用のスレッドを N 本**（`[api] python_event_loops`、既定 1）起動し、それぞれ
  `loop.run_forever()` を回す。`uvloop` が入っていれば使う（`[api] python_event_loop = "auto" | "asyncio" | "uvloop"`）。
- 投入: ワーカーは「呼び出し」（ハンドラの id・要求のネイティブのオブジェクトの元になる Rust の値・結果のスロット）を
  ループごとの SPSC 風の有界キュー（MPSC。ワーカー → ループ）に積み、ループの `eventfd`（Windows・macOS は自分宛ての
  ソケットの組）に書く。ループは `loop.add_reader(fd, drain)` でキューを抜き、`loop.create_task(handler(...))` する。
  ループのスレッドは GIL を持ったまま回るので、ワーカーが GIL を取る場面は無い。
- 完了: タスクの `done` で応答（`veil.Response` 等をネイティブの応答へ変換したもの）を結果のスロットに入れ、ワーカーの通知 fd を
  鳴らす（`await_completion`）。ストリーミング応答（非同期ジェネレータ）は F-210 のパイプ（`api::pipe`）へループから書く。
- 振り分け: 要求ごとにループを選ぶ（接続の fd の番号で固定。同じ接続の要求・WebSocket のメッセージは同じループへ）。
- キャンセル: クライアントの切断でワーカー側の Future が drop されたら、キャンセルをキューで送り `task.cancel()`。
- 背圧: キューが満杯ならワーカーは 503 を返す（`[api] python_event_loop_queue`、既定 65,536）。黙って待たない。
- `async def` のハンドラの既定の実行先を **常駐のループ** にする（FastAPI・uvicorn と同じ意味。ループを止める同期の処理を
  `async def` に書くと全体が止まることを文書に書く）。旧来の「プールでの `run_until_complete`」は
  `@app.get(..., async_mode="pool")`・`[api] python_async_mode = "pool"` で選べるように残す（移行用）。
- 同期のハンドラは今のままプール（`offload=True`）か I/O のワーカー（`offload=False`）。ループから同期の重い処理を逃がす
  `await veil.run_in_threadpool(fn, ...)`（ハンドラ用プールで実行。Starlette と同じ名前）を用意する。

### 2. lifespan・起動と終了の処理

- `App(lifespan=async_context_manager)`（FastAPI と同じ形）・`@app.on_startup` / `@app.on_shutdown`（`on_event` 互換の別名も）。
  起動時はリスナーを開く前にループ 0 で `__aenter__` まで実行し、例外なら起動を止める（終了コード 1。B-161 と同じ）。
  終了時（graceful shutdown の開始）に `__aexit__`。`app.state` を lifespan で作ったものの置き場にする。

### 3. WebSocket（`@app.websocket`）

```python
@app.websocket("/ws")
async def ws(websocket: veil.WebSocket):
    await websocket.accept()                       # subprotocol=..., headers=...
    async for text in websocket.iter_text():
        await websocket.send_text(text)
```

- API は Starlette / FastAPI の `WebSocket` と同じ名前にする: `accept`・`receive_text` / `receive_bytes` / `receive_json` /
  `receive`・`send_text` / `send_bytes` / `send_json`・`iter_text` / `iter_bytes`・`close(code, reason)`・`client`・`headers`・
  `query_params`・`path_params`・`url`・`state`、切断は `veil.WebSocketDisconnect(code)`。パス引数・クエリ引数の束縛は今のルートと同じ。
- 中身: ハンドシェイク・フレーム・Ping/Pong・UTF-8 の検査・上限は **Rust のワーカーで**（F-262 の codec）。Python に渡るのは
  組み立て済みのメッセージだけ（`str` / `bytes`）。受信はワーカー → ループのキュー、送信はループ → ワーカーのキュー
  （どちらも有界。受信側が詰まったら読み取りを止める＝ TCP の背圧）。`send_*` は送信キューに積んで返り、
  まとめて届いた分はワーカーが 1 回の `writev` で送る（echo-ws-pipeline）。
- `accept()` を呼ばずに返したら 403（Starlette と同じ）。`accept()` までハンドシェイクの応答を保留する
  （ワーカーは 101 を書かずに待つ。F-261 の `switching_protocols` を「承認を待つ」形で使う）。
- 同期の書き方: `class Echo(veil.WebSocketEndpoint)` の `on_connect` / `on_receive` / `on_disconnect`（Starlette と同じ名前）を
  ハンドラ用プールで 1 本ずつ呼ぶ（L4 の `veil.Protocol` と同じイベント型。プールを長く占有しない）。
- OpenAPI には載せない（FastAPI と同じ）。

### 4. プロトコルアップグレード（WebSocket 以外）

- `return veil.Response.switching_protocols("x-proto", factory)` で、`factory()` が返す `veil.Protocol`
  （`asyncio.Protocol` と同じ名前の `connection_made` / `data_received` / `connection_lost`、既存の L4 と同じ）で受ける。
  非同期で書くときは `veil.AsyncProtocol`（`async def data_received` をループで実行）。

### 5. 設定（`[api]`、ライブラリ専用）

| キー | 既定 | 意味 |
|---|---|---|
| `python_event_loops` | 1 | 常駐のループの本数 |
| `python_event_loop` | `"auto"` | `uvloop` があれば使う |
| `python_event_loop_queue` | 65536 | 1 ループの投入キューの長さ（満杯で 503） |
| `python_async_mode` | `"loop"` | `async def` の実行先（`"pool"` = 旧来） |
| WebSocket の各キー | F-262 と共通 | `websocket_max_message_size` 等 |

`veil.run(..., event_loops=2)` のように引数でも渡せる。

## 試験

- `crates/veil-python/tests/`: ループへの投入・完了・キャンセル（切断で `CancelledError`）・例外の 500・キュー満杯の 503・
  lifespan の順序と失敗時の終了コード・`run_in_threadpool`。
- E2E（`tests/api/library_e2e.sh python`）: `asyncio.sleep(1)` の要求を 32,768 同時に流して全部 1 秒台で返る、WebSocket の
  エコー（16,384 接続・16 件まとめ送り・10 メッセージごとの切断）、`WebSocketEndpoint`、`accept` しないと 403、
  アップグレードの独自プロトコル、Autobahn の fuzzingclient（Python のエコーで 1.*〜7.*）。
- 互換: FastAPI の `WebSocket` のサンプル（公式文書のチャット）を import の差し替えだけで動かす。
- 性能（`tools/api-bench/python`）: `async def` の `/plaintext`・`/json` を旧来（`pool`）・常駐のループ・FastAPI + uvicorn で交互に比べる
  （常駐のループが旧来より遅くならないこと。遅いなら既定を見直す）。同期のハンドラの数値が変わらないこと。

## 文書

- `crates/veil-python/docs/`（英日）: `websocket.md` を書き直し（API・同期の書き方・設定）、`async.md`（新規。ループ・
  `run_in_threadpool`・lifespan・`async_mode`）、`quickstart.md` の FastAPI 対応表（WebSocket・lifespan を「あり」に）。
- `docs/guide/library.md`（英日）の Python の節と Limitations。リリースノートに `async def` の既定の実行先の変更。

## 範囲外

- free-threaded の CPython（3.13t / 3.14t。abi3 の wheel が使えない）。サブインタプリタ。

## 受け入れ条件

- 上の E2E が通り、同期のハンドラの性能が変わらない。本体のバイナリが変わらない（`nonregression`）。
- 文書（英日）・`backlog.md` が更新されている。
