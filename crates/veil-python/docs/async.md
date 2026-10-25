# async handlers, the event loop and lifespan

## The persistent event loop

`async def` handlers run on a **persistent asyncio event loop** in a thread of its own, as with uvicorn: a request
that awaits (`asyncio.sleep`, an async database or HTTP client) does not hold a thread while it waits, so
thousands of requests can wait at once, and tasks created with `asyncio.create_task` keep running after the
request. The network workers never take the GIL: they queue the call for the loop and are woken when the response
is ready.

```python
@app.get("/delay/{ms}")
async def delay(ms: int):
    await asyncio.sleep(ms / 1000)
    return {"slept": ms}
```

**Do not block the loop.** Blocking code in an `async def` handler (`time.sleep`, a synchronous database driver,
heavy computation) stalls every request and WebSocket served by that loop. Move it off the loop:

```python
@app.get("/report")
async def report():
    data = await veil.run_in_threadpool(build_report, year=2026)   # same name as Starlette's
    return data
```

or write the handler as a plain `def` (it then runs on the handler pool, as before).

Settings (`[api]` in the configuration, or arguments of `run`):

| Key | `run(...)` argument | Default | Meaning |
|-----|---------------------|---------|---------|
| `python_event_loops` | `event_loops` | `1` | number of persistent loops (requests are spread over them) |
| `python_event_loop` | `event_loop` | `"auto"` | `"auto"` uses uvloop when it is installed, `"asyncio"` never does, `"uvloop"` requires it |
| `python_event_loop_queue` | | `65536` | calls that may wait for one loop; when it is full veil answers 503 |
| `python_async_mode` | `async_mode` | `"loop"` | where `async def` handlers run: `"loop"` (persistent loop) or `"pool"` |

- When the client disconnects before the response, the handler's task is cancelled (`asyncio.CancelledError`).
- An exception in the handler gives 500 (and is printed); `HTTPException` and validation errors behave as usual.
- Streaming responses (async generators, `veil.StreamingResponse`, `veil.EventSourceResponse`) are iterated on the
  loop; the worker's queue applies backpressure.

**`async_mode="pool"`** (per route, `@app.get(..., async_mode="pool")`, or for all routes with
`python_async_mode = "pool"`) keeps the behaviour of veil before 0.10: the coroutine is run to completion on a
handler-pool thread with an event loop per thread, so it holds that thread while it waits. Routes with
`stream_body=True` always run this way (`request.stream()` reads the body synchronously).

## Lifespan, startup and shutdown

```python
import contextlib

@contextlib.asynccontextmanager
async def lifespan(app):
    pool = await asyncpg.create_pool(DSN)
    yield {"pool": pool}                 # a dict is merged into app.state
    await pool.close()

app = veil.App(lifespan=lifespan)

@app.get("/users/{id}")
async def user(id: int):
    async with app.state.pool.acquire() as c:
        return dict(await c.fetchrow("select * from users where id = $1", id))
```

- The part of `lifespan` before `yield` runs on the first event loop **before the listeners open**; if it raises,
  veil prints the error and exits with status 1. The part after `yield` runs after the server stopped
  (graceful shutdown finished), so the resources are still there while the last requests complete.
- `@app.on_startup` / `@app.on_shutdown` (and FastAPI's `@app.on_event("startup" | "shutdown")`) register plain or
  `async def` functions run after the lifespan's start and before its end. `App(on_startup=[...], on_shutdown=[...])`
  also works.
- Objects created there (database pools, `redis.asyncio` clients, `httpx.AsyncClient`) belong to the event loop:
  use them from `async def` handlers. With several loops (`python_event_loops > 1`) they are created on the first
  loop; prefer one loop, or create per-loop clients lazily.
