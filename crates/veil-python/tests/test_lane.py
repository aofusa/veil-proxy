"""スレッドの実行線（F-299）の試験: 本物のサーバを子プロセスで起動して、ブロッキングなハンドラの並行性・GIL の
引き継ぎ・要求の間の分離・停止を確かめる。

    python -I tests/test_lane.py        # ネイティブ拡張（maturin develop）が要る。無ければ飛ばす
"""

import concurrent.futures
import contextvars
import http.client
import json
import os
import signal
import socket
import subprocess
import sys
import threading
import time
import unittest

try:
    from veil import _veil  # noqa: F401

    HAVE_NATIVE = True
except ImportError:
    HAVE_NATIVE = False

THREADS = 3


def serve(port):
    import veil

    var = contextvars.ContextVar("var", default="unset")
    local = threading.local()
    idents = set()

    @veil.get("/quick")
    def quick():
        return "ok"

    @veil.get("/sleep/{ms}")
    def sleep_(ms: int):
        time.sleep(ms / 1000)
        return {"slept": ms}

    @veil.get("/spin/{ms}")
    def spin(ms: int):
        end = time.monotonic() + ms / 1000
        while time.monotonic() < end:
            pass
        return {"spun": ms}

    @veil.get("/context")
    def context():
        seen = var.get()
        var.set("set by an earlier request")
        return {"seen": seen}

    @veil.get("/local")
    def local_():
        local.n = getattr(local, "n", 0) + 1
        idents.add(threading.get_ident())
        return {"n": local.n, "threads": len(idents)}

    @veil.get("/boom")
    def boom():
        raise RuntimeError("expected by the test")

    veil.run(
        listen_cleartext=f"127.0.0.1:{port}",
        threads=2,
        overrides={"logging.level": "error", "access_log.enabled": False, "api.handler_threads": THREADS},
        openapi=False,
    )
    print("stopped", flush=True)


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


@unittest.skipUnless(HAVE_NATIVE, "native extension not built")
class Lane(unittest.TestCase):
    def setUp(self):
        self.port = free_port()
        self.srv = subprocess.Popen(
            [sys.executable, "-I", os.path.abspath(__file__), "--serve", str(self.port)],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        self.addCleanup(self.stop_server)
        deadline = time.monotonic() + 15
        while True:
            try:
                self.get("/quick")
                return
            except OSError:
                if self.srv.poll() is not None or time.monotonic() > deadline:
                    self.fail("the server did not start")
                time.sleep(0.05)

    def stop_server(self):
        if self.srv.poll() is None:
            self.srv.kill()
        self.srv.communicate()

    def get(self, path, timeout=30):
        c = http.client.HTTPConnection("127.0.0.1", self.port, timeout=timeout)
        try:
            c.request("GET", path)
            r = c.getresponse()
            return r.status, r.read()
        finally:
            c.close()

    def test_blocking_handlers_run_up_to_the_thread_count(self):
        n = THREADS * 2
        start = time.monotonic()
        with concurrent.futures.ThreadPoolExecutor(n) as ex:
            got = list(ex.map(lambda _: self.get("/sleep/300"), range(n)))
        took = time.monotonic() - start
        self.assertEqual([s for s, _ in got], [200] * n)
        self.assertGreaterEqual(took, 0.55, "more handlers ran at once than there are threads")
        self.assertLess(took, 300 * n / 1000, "the handlers ran one at a time")

    def test_a_sleeping_handler_does_not_hold_up_others(self):
        with concurrent.futures.ThreadPoolExecutor(1) as ex:
            slow = ex.submit(self.get, "/sleep/1000")
            time.sleep(0.1)
            start = time.monotonic()
            self.assertEqual(self.get("/quick")[0], 200)
            self.assertLess(time.monotonic() - start, 0.5)
            self.assertEqual(slow.result()[0], 200)

    def test_a_handler_holding_the_gil_does_not_hold_up_others(self):
        with concurrent.futures.ThreadPoolExecutor(1) as ex:
            slow = ex.submit(self.get, "/spin/1000")
            time.sleep(0.1)
            start = time.monotonic()
            self.assertEqual(self.get("/quick")[0], 200)
            self.assertLess(time.monotonic() - start, 0.5)
            self.assertEqual(slow.result()[0], 200)

    def test_all_threads_blocked_then_the_queue_moves_again(self):
        with concurrent.futures.ThreadPoolExecutor(THREADS + 4) as ex:
            slow = [ex.submit(self.get, "/sleep/400") for _ in range(THREADS)]
            time.sleep(0.1)
            quick = [ex.submit(self.get, "/quick") for _ in range(4)]
            for f in slow + quick:
                self.assertEqual(f.result()[0], 200)

    def test_context_variables_do_not_leak_between_requests(self):
        for _ in range(THREADS * 8):
            status, body = self.get("/context")
            self.assertEqual(status, 200)
            self.assertEqual(json.loads(body), {"seen": "unset"})

    def test_threads_are_kept_between_requests(self):
        for _ in range(THREADS * 8):
            status, body = self.get("/local")
            self.assertEqual(status, 200)
        got = json.loads(body)
        self.assertLessEqual(got["threads"], THREADS)
        self.assertGreater(got["n"], 1, "thread-local data did not survive between requests")

    def test_an_exception_answers_500_and_keeps_the_threads(self):
        for _ in range(THREADS * 4):
            self.assertEqual(self.get("/boom")[0], 500)
        self.assertEqual(self.get("/quick")[0], 200)

    def test_many_concurrent_requests_all_answer(self):
        with concurrent.futures.ThreadPoolExecutor(32) as ex:
            got = list(ex.map(lambda i: self.get("/quick" if i % 7 else "/sleep/5")[0], range(2000)))
        self.assertEqual(got, [200] * 2000)

    @unittest.skipIf(os.name == "nt", "no graceful stop by signal")
    def test_stop_finishes_the_request_in_flight_and_exits_cleanly(self):
        with concurrent.futures.ThreadPoolExecutor(1) as ex:
            slow = ex.submit(self.get, "/sleep/500")
            time.sleep(0.2)
            self.srv.send_signal(signal.SIGTERM)
            self.assertEqual(slow.result()[0], 200)
        out, _ = self.srv.communicate(timeout=20)
        self.assertEqual(self.srv.returncode, 0)
        self.assertIn("stopped", out)


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--serve":
        serve(int(sys.argv[2]))
    else:
        unittest.main()
