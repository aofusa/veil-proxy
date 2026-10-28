"""Persistent asyncio event loops that run ``async def`` handlers and WebSocket sessions.

The network workers (Rust) never take the GIL: they push jobs onto a bounded queue per loop and write one byte to
the loop's socket pair. The loop reads the socket (``loop.add_reader``), takes the jobs with ``_veil.aio_take`` and
runs them as tasks; results go back to the worker through a queue and the worker's notification fd.
"""

import asyncio
import concurrent.futures
import functools
import os
import socket
import sys
import threading
import traceback

# ジョブの種類（crates/veil-python/src/aio.rs の JOB_* と同じ値）
JOB_CALL = 0
JOB_CANCEL = 1
JOB_WS = 2
JOB_WAKE = 3

_LOOPS = []
_POOL = []
_POOL_LOCK = threading.Lock()


def _new_loop(kind):
    """Create an event loop: uvloop when ``kind`` is ``"uvloop"`` (required) or ``"auto"`` (if installed).

    Otherwise an asyncio loop; on Windows a ``SelectorEventLoop``, since the default proactor loop has no
    ``add_reader``.
    """
    if kind not in ("auto", "asyncio", "uvloop"):
        raise ValueError("python_event_loop must be \"auto\", \"asyncio\" or \"uvloop\", not %r" % (kind,))
    if kind != "asyncio":
        try:
            import uvloop  # noqa: PLC0415

            return uvloop.new_event_loop()
        except ImportError:
            if kind == "uvloop":
                raise
    # Windows の既定（ProactorEventLoop）は add_reader を持たない。起床のソケットを読むので selector のループにする
    if sys.platform == "win32":
        return asyncio.SelectorEventLoop()
    return asyncio.new_event_loop()


class EventLoop:
    """One persistent loop and its thread."""

    def __init__(self, index, kind):
        self.index = index
        self.loop = _new_loop(kind)
        self.tasks = {}  # 呼び出しの id → Task（切断で cancel する）
        self.sockets = {}  # WebSocket の id → veil.WebSocket（受信の起床を届ける）
        self.rsock, self.wsock = socket.socketpair()
        self.rsock.setblocking(False)
        self.wsock.setblocking(False)
        self._ready = threading.Event()
        self.thread = threading.Thread(target=self._main, name="veil-aio-%d" % index, daemon=True)

    def _main(self):
        asyncio.set_event_loop(self.loop)
        self.loop.add_reader(self.rsock.fileno(), self._drain)
        self.loop.call_soon(self._ready.set)
        try:
            self.loop.run_forever()
        finally:
            self.loop.remove_reader(self.rsock.fileno())

    def start(self):
        self.thread.start()
        self._ready.wait()

    def _drain(self):
        from . import _veil

        try:
            while self.rsock.recv(4096):
                pass
        except (BlockingIOError, InterruptedError):
            pass
        for job in _veil.aio_take(self.index):
            kind = job.kind
            try:
                if kind == JOB_CALL:
                    self.tasks[job.id] = self.loop.create_task(_run_call(self, job))
                elif kind == JOB_CANCEL:
                    t = self.tasks.pop(job.id, None)
                    if t is not None:
                        t.cancel()
                elif kind == JOB_WS:
                    from .websockets import _run_ws

                    self.loop.create_task(_run_ws(self, job))
                elif kind == JOB_WAKE:
                    ws = self.sockets.get(job.id)
                    if ws is not None:
                        ws._wake()
            except Exception:  # noqa: BLE001  1 件の失敗で他のジョブを落とさない
                traceback.print_exc()

    def run_sync(self, coro, timeout=None):
        """Run ``coro`` on this loop from another thread and wait for its result."""
        return asyncio.run_coroutine_threadsafe(coro, self.loop).result(timeout)

    def stop(self):
        if self.loop.is_running():
            self.loop.call_soon_threadsafe(self.loop.stop)
        self.thread.join(5)
        # B-207: 起床のソケットを閉じる（Rust 側の登録は先に aio_teardown で外してある）
        if not self.thread.is_alive():
            self.rsock.close()
            self.wsock.close()


async def _run_call(el, job):
    """Run one ``async def`` handler call and hand the result to the worker."""
    from . import _HANDLERS, StreamingResponse, _stream_of

    route = _HANDLERS[job.handler_id]
    try:
        result = await route._acall(job.request)
        stream = result if isinstance(result, StreamingResponse) else _stream_of(result, route.status_code)
        if stream is not None:
            await _stream_out(job, stream)
        else:
            job.complete(result)
    except asyncio.CancelledError:
        job.cancelled()
    except Exception:  # noqa: BLE001  500 にする（サーバは止めない）
        traceback.print_exc()
        job.fail()
    finally:
        el.tasks.pop(job.id, None)


async def _stream_out(job, sr):
    """Send a streaming response from the loop (waits while the worker's queue is full)."""
    w = job.start_stream(sr)
    agen = sr._aiter()
    try:
        async for item in agen:
            if not item:
                continue
            while not w.try_write(item):
                if w.closed:
                    return
                await asyncio.sleep(0.001)
    finally:
        aclose = getattr(agen, "aclose", None)
        if aclose is not None and w.closed:
            await aclose()
    w.finish(sr.trailers)
    # F-268: 送り終えた後の background（BackgroundTasks 等）
    bg = getattr(sr, "background", None)
    if bg is not None:
        from . import _arun_background

        await _arun_background(bg)


def start(n, kind):
    """Start ``n`` loops (once per process) and return the write ends of their socket pairs."""
    if _LOOPS:
        return [el.wsock.fileno() for el in _LOOPS]
    for i in range(max(1, int(n))):
        el = EventLoop(i, kind)
        el.start()
        _LOOPS.append(el)
    return [el.wsock.fileno() for el in _LOOPS]


def first():
    """The first loop (lifespan and startup / shutdown handlers run on it)."""
    return _LOOPS[0] if _LOOPS else None


def stop():
    """Stop the loops (after the server stopped)."""
    # B-207: 先に Rust 側の登録を外す（以後の投入は断られ、閉じたソケットへ合図を書かない）。次の App.run は
    # 新しいループを登録できる
    try:
        from . import _veil

        _veil.aio_teardown()
    except ImportError:
        pass
    while _LOOPS:
        _LOOPS.pop().stop()


def _executor():
    with _POOL_LOCK:
        if not _POOL:
            _POOL.append(
                concurrent.futures.ThreadPoolExecutor(
                    max_workers=min(64, (os.cpu_count() or 1) + 4), thread_name_prefix="veil-threadpool"
                )
            )
        return _POOL[0]


async def run_in_threadpool(func, *args, **kwargs):
    """Run the blocking ``func(*args, **kwargs)`` on a thread pool and await its result (same name as Starlette's).

    Use it from an ``async def`` handler for blocking work (file I/O, CPU-bound code, synchronous clients):
    blocking the persistent event loop stalls every request and WebSocket served by it.
    """
    loop = asyncio.get_running_loop()
    return await loop.run_in_executor(_executor(), functools.partial(func, *args, **kwargs))
