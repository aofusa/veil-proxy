#!/usr/bin/env python3
"""veil のハンドラ経路の 1 要求あたりの確保回数（allocs/req）を測る（F-202）。

    (cd tools/api-bench/axum-veil && CARGO_TARGET_DIR=target-count cargo build --release --features count-allocs)
    python3 tools/api-bench/allocs.py [--bin PATH] [--mode native|mount] [--path /plaintext]

`count-allocs` のビルドは確保の累計を 1 秒ごとに標準エラーへ出す。wrk で負荷をかける前後の累計の差を、
wrk が数えた要求数で割る（キープアライブ。接続の確立ぶんは負荷の前に済ませる）。
"""
import argparse
import os
import re
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
# 待ち受け（run_bench.py と同じ BENCH_PORT。既定 3000）
ADDR = "127.0.0.1:%s" % os.environ.get("BENCH_PORT", "3000")
WRK_IMG = os.environ.get("WRK_IMG", "local/wrk-openssl")


def last_count(path):
    with open(path) as f:
        xs = re.findall(r"^allocs (\d+)$", f.read(), re.M)
    return int(xs[-1]) if xs else None


def wrk(url, secs, conns):
    out = subprocess.run(["docker", "run", "--rm", "--network", "host", WRK_IMG, "-t2", f"-c{conns}",
                          f"-d{secs}s", url], capture_output=True, text=True).stdout
    m = re.search(r"(\d+) requests in", out)
    return int(m.group(1)) if m else 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=os.path.join(HERE, "axum-veil", "target-count", "release", "api-bench-axum-veil"))
    ap.add_argument("--mode", default="native")
    ap.add_argument("--path", default="/plaintext")
    ap.add_argument("--conns", type=int, default=32)
    ap.add_argument("--secs", type=int, default=5)
    a = ap.parse_args()
    log = "/tmp/veil-allocs-%d.log" % os.getpid()
    with open(log, "w") as f:
        p = subprocess.Popen([a.bin, a.mode, ADDR, "2"], stdout=subprocess.DEVNULL, stderr=f)
    try:
        time.sleep(2)
        if p.poll() is not None:
            sys.exit(f"server exited at startup (is {ADDR} in use? set BENCH_PORT); see {log}")
        url = "http://" + ADDR + a.path
        wrk(url, 2, a.conns)                      # ウォームアップ（プール・接続）
        time.sleep(1.2)
        before = last_count(log)
        n = wrk(url, a.secs, a.conns)
        time.sleep(1.2)
        after = last_count(log)
        print(f"{a.mode} {a.path}: requests={n} allocs={after - before} allocs/req={(after - before) / max(n, 1):.2f}")
    finally:
        p.terminate()
        p.wait()
        os.remove(log)


if __name__ == "__main__":
    main()
