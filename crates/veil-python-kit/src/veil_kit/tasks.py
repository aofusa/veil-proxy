"""Application tasks: periodic jobs, cron schedules and resident coroutines.

.. code-block:: python

    from veil_kit.tasks import Tasks

    tasks = Tasks(app)

    @tasks.every(seconds=30)
    async def refresh():
        ...

    @tasks.cron("*/5 * * * *")          # minute hour day month weekday (UTC); croniter is used when installed
    def report():
        ...

    @tasks.background
    async def consume():
        while True:
            ...

Tasks start after the application startup (on the first event loop) and are cancelled when the server stops.
``async def`` tasks run on the event loop; plain functions run on a thread pool. Runs of one task never overlap; an
exception is logged and the task keeps its schedule. For work tied to one request use ``veil.BackgroundTasks``.
"""

# F-268: 常駐のループ（F-263）の上で動かす。lifespan の起動の後に始め、停止で取り消す。

import asyncio
import datetime
import inspect
import random
import traceback

import veil

__all__ = ["Tasks", "CronSchedule"]


class Tasks:
    """The tasks of an application (registered on ``app.on_startup`` / ``app.on_shutdown``)."""

    def __init__(self, app=None):
        """Attach to ``app`` (``veil``'s default application when omitted)."""
        self.app = app if app is not None else veil._default_app
        self._specs = []
        self._running = []
        self.app.on_startup(self._start)
        self.app.on_shutdown(self._stop)

    def every(self, seconds=None, *, minutes=None, hours=None, jitter=0.1, run_at_start=False):
        """Run the decorated function every period (the first run after up to ``jitter`` of a period, or at start)."""
        period = (seconds or 0) + 60 * (minutes or 0) + 3600 * (hours or 0)
        if period <= 0:
            raise ValueError("the period must be positive")

        def deco(f):
            self._specs.append(("every", f, period, jitter, run_at_start))
            return f

        return deco

    def cron(self, expr):
        """Run the decorated function on a cron schedule (five fields, UTC)."""
        sched = CronSchedule(expr)

        def deco(f):
            self._specs.append(("cron", f, sched))
            return f

        return deco

    def background(self, f):
        """Run the decorated coroutine function once at start and keep it running until the server stops."""
        self._specs.append(("background", f))
        return f

    async def _start(self):
        loop = asyncio.get_running_loop()
        for spec in self._specs:
            self._running.append(loop.create_task(self._run(spec)))

    async def _stop(self):
        for t in self._running:
            t.cancel()
        for t in self._running:
            try:
                await t
            except (asyncio.CancelledError, Exception):  # noqa: BLE001
                pass
        self._running.clear()

    @staticmethod
    async def _once(f):
        try:
            if inspect.iscoroutinefunction(f):
                await f()
            else:
                await veil.run_in_threadpool(f)
        except asyncio.CancelledError:
            raise
        except Exception:  # noqa: BLE001  1 回の失敗で予定を止めない
            traceback.print_exc()

    async def _run(self, spec):
        kind = spec[0]
        if kind == "background":
            await self._once(spec[1])
            return
        if kind == "every":
            _, f, period, jitter, at_start = spec
            loop = asyncio.get_running_loop()
            first = 0 if at_start else random.uniform(0, period * jitter)
            nxt = loop.time() + first
            while True:
                await asyncio.sleep(max(0.0, nxt - loop.time()))
                await self._once(f)
                nxt += period
                if nxt < loop.time():  # 遅れたら次の周期へ（重ねない）
                    nxt = loop.time() + period
        _, f, sched = spec
        while True:
            now = datetime.datetime.now(datetime.timezone.utc)
            nxt = sched.next_after(now)
            await asyncio.sleep(max(0.0, (nxt - now).total_seconds()))
            await self._once(f)


class CronSchedule:
    """A five-field cron expression (``minute hour day month weekday``; ``*``, numbers, ``a-b``, lists and ``/step``;
    weekday 0 or 7 = Sunday). Uses croniter when it is installed."""

    _RANGES = ((0, 59), (0, 23), (1, 31), (1, 12), (0, 7))

    def __init__(self, expr):
        """Parse ``expr`` (``ValueError`` when invalid)."""
        self.expr = expr
        try:
            import croniter  # noqa: PLC0415

            if not croniter.croniter.is_valid(expr):
                raise ValueError("invalid cron expression %r" % expr)
            self._croniter = croniter.croniter
        except ImportError:
            self._croniter = None
        fields = expr.split()
        if len(fields) != 5:
            raise ValueError("a cron expression has five fields: %r" % expr)
        self.sets = [self._field(f, lo, hi) for f, (lo, hi) in zip(fields, self._RANGES)]
        if 7 in self.sets[4]:
            self.sets[4].add(0)
        self.dom_any = fields[2] == "*"
        self.dow_any = fields[4] == "*"

    @staticmethod
    def _field(text, lo, hi):
        out = set()
        for part in text.split(","):
            rng, _, step = part.partition("/")
            step = int(step) if step else 1
            if step <= 0:
                raise ValueError("bad step in %r" % text)
            if rng == "*":
                a, b = lo, hi
            elif "-" in rng:
                a, b = (int(x) for x in rng.split("-", 1))
            else:
                a = int(rng)
                b = hi if step > 1 else a
            if not (lo <= a <= hi and lo <= b <= hi and a <= b):
                raise ValueError("value out of range in %r" % text)
            out.update(range(a, b + 1, step))
        return out

    def next_after(self, t):
        """The first run time after ``t`` (an aware ``datetime``)."""
        if self._croniter is not None:
            return self._croniter(self.expr, t).get_next(datetime.datetime)
        mins, hours, days, months, dows = self.sets
        t = (t + datetime.timedelta(minutes=1)).replace(second=0, microsecond=0)
        for _ in range(366 * 24 * 60):
            dow = (t.weekday() + 1) % 7
            if self.dom_any and self.dow_any:
                day_ok = True
            elif self.dom_any:
                day_ok = dow in dows
            elif self.dow_any:
                day_ok = t.day in days
            else:
                day_ok = t.day in days or dow in dows
            if t.month in months and day_ok and t.hour in hours and t.minute in mins:
                return t
            if t.month not in months or not day_ok:
                t = (t + datetime.timedelta(days=1)).replace(hour=0, minute=0)
            elif t.hour not in hours:
                t = (t + datetime.timedelta(hours=1)).replace(minute=0)
            else:
                t += datetime.timedelta(minutes=1)
        raise ValueError("no run time within a year for %r" % self.expr)
