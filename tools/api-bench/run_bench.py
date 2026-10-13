#!/usr/bin/env python3
"""API サーバの比較ベンチマーク（axum + tokio / axum + veil の tokio 互換 / veil、FastAPI / veil の Python）。

サーバを CPU 0-1（taskset）、負荷ツール wrk を CPU 2-3（docker --cpuset-cpus）に分け、対象をラウンドごとに
順番を入れ替えて交互に測る（時間をまたいだ比較にしない）。結果は TSV（1 行 = 1 回の計測）と、中央値の Markdown 表。

    python3 tools/api-bench/run_bench.py --group rust   --rounds 3 --out docs/artifacts/api_bench_rust.tsv
    python3 tools/api-bench/run_bench.py --group python --rounds 3 --out docs/artifacts/api_bench_python.tsv
    python3 tools/api-bench/run_bench.py --summary docs/artifacts/api_bench_rust.tsv

前提: tools/api-bench/README.md のビルド手順（axum-tokio・axum-veil の release ビルド、Python の venv）と
docker イメージ local/wrk-openssl（wrk）。
"""

import argparse
import csv
import os
import re
import signal
import socket
import statistics
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", ".."))
VENV = os.environ.get("BENCH_VENV", os.path.join(ROOT, "target", "py-bench-venv"))
ADDR = "127.0.0.1:3000"
SERVER_CPUS = os.environ.get("BENCH_SERVER_CPUS", "0,1")
CLIENT_CPUS = os.environ.get("BENCH_CLIENT_CPUS", "2,3")
THREADS = int(os.environ.get("BENCH_THREADS", "2"))
WRK_IMG = os.environ.get("WRK_IMG", "local/wrk-openssl:latest")
PATHS = {"plaintext": "/plaintext", "json": "/json/veil"}

AT = os.path.join(HERE, "axum-tokio", "target", "release", "api-bench-axum-tokio")
AV = os.path.join(HERE, "axum-veil", "target", "release", "api-bench-axum-veil")
# reactor（epoll）版: cd axum-veil && CARGO_TARGET_DIR=target-epoll cargo build --release --features epoll
AVE = os.path.join(HERE, "axum-veil", "target-epoll", "release", "api-bench-axum-veil")
PY = os.path.join(VENV, "bin", "python")
UV = os.path.join(VENV, "bin", "uvicorn")
PYDIR = os.path.join(HERE, "python")
UVARGS = ["--host", "127.0.0.1", "--port", "3000", "--log-level", "warning", "--no-access-log"]

# 名前 → (コマンド, 追加の環境変数, 作業ディレクトリ)
TARGETS = {
    "rust": {
        "axum+tokio (serve)": ([AT, "serve", ADDR, str(THREADS)], {}, None),
        "axum+tokio (percore)": ([AT, "percore", ADDR, str(THREADS)], {}, None),
        "axum+veil::tokio (serve)": ([AV, "serve", ADDR, str(THREADS)], {}, None),
        "axum+veil::tokio (percore)": ([AV, "percore", ADDR, str(THREADS)], {}, None),
        "veil App + axum Router (mount)": ([AV, "mount", ADDR, str(THREADS)], {}, None),
        "veil native handler": ([AV, "native", ADDR, str(THREADS)], {}, None),
        "axum+veil::tokio (serve) [epoll]": ([AVE, "serve", ADDR, str(THREADS)], {}, None),
        "axum+veil::tokio (percore) [epoll]": ([AVE, "percore", ADDR, str(THREADS)], {}, None),
        "veil App + axum Router (mount) [epoll]": ([AVE, "mount", ADDR, str(THREADS)], {}, None),
        "veil native handler [epoll]": ([AVE, "native", ADDR, str(THREADS)], {}, None),
    },
    "python": {
        "FastAPI async, uvicorn 1 worker": ([UV, "fastapi_app:app", "--workers", "1"] + UVARGS, {}, PYDIR),
        "FastAPI async, uvicorn 2 workers": ([UV, "fastapi_app:app", "--workers", "2"] + UVARGS, {}, PYDIR),
        "FastAPI def, uvicorn 2 workers": ([UV, "fastapi_app:app", "--workers", "2"] + UVARGS, {"BENCH_SYNC": "1"}, PYDIR),
        "veil (offload=True)": ([PY, "veil_app.py"], {"BENCH_OFFLOAD": "1", "BENCH_THREADS": str(THREADS)}, PYDIR),
        "veil (offload=False)": ([PY, "veil_app.py"], {"BENCH_OFFLOAD": "0", "BENCH_THREADS": str(THREADS)}, PYDIR),
    },
}


def tree_pids(pid):
    out = [pid]
    for p in out:
        try:
            r = subprocess.run(["pgrep", "-P", str(p)], capture_output=True, text=True)
            out += [int(x) for x in r.stdout.split()]
        except Exception:
            pass
    return out


def cpu_ticks(pids):
    t = 0
    for p in pids:
        try:
            f = open(f"/proc/{p}/stat").read().rsplit(")", 1)[1].split()
            t += int(f[11]) + int(f[12])  # utime + stime
        except OSError:
            pass
    return t


def rss_mb(pids):
    kb = 0
    for p in pids:
        try:
            m = re.search(r"VmRSS:\s+(\d+)", open(f"/proc/{p}/status").read())
            kb += int(m.group(1)) if m else 0
        except OSError:
            pass
    return kb / 1024


def cores_busy(cpus):
    """サーバに割り当てたコアの busy 時間（tick。ソフト割り込み等のカーネル処理を含む）。"""
    want = {f"cpu{c}" for c in cpus.split(",")}
    t = 0
    for line in open("/proc/stat"):
        f = line.split()
        if f[0] in want:
            v = [int(x) for x in f[1:]]
            t += sum(v) - v[3] - v[4]  # idle と iowait を除く
    return t


def wait_port(timeout=20):
    end = time.time() + timeout
    while time.time() < end:
        try:
            socket.create_connection(("127.0.0.1", 3000), timeout=0.5).close()
            return True
        except OSError:
            time.sleep(0.2)
    return False


def wrk(path, duration, conns):
    cmd = ["docker", "run", "--rm", "--network", "host", "--cpuset-cpus", CLIENT_CPUS, WRK_IMG,
           f"-t{len(CLIENT_CPUS.split(','))}", f"-c{conns}", f"-d{duration}s", "--latency", f"http://{ADDR}{path}"]
    r = subprocess.run(cmd, capture_output=True, text=True, timeout=duration + 60)
    o = r.stdout
    rps = re.search(r"Requests/sec:\s+([\d.]+)", o)
    p50 = re.search(r"\s50%\s+([\d.]+)(us|ms|s)", o)
    p99 = re.search(r"\s99%\s+([\d.]+)(us|ms|s)", o)
    errs = re.search(r"Socket errors: (.*)", o)
    non2xx = re.search(r"Non-2xx or 3xx responses: (\d+)", o)

    def ms(m):
        if not m:
            return "NA"
        v = float(m.group(1))
        return f"{v / 1000 if m.group(2) == 'us' else v * 1000 if m.group(2) == 's' else v:.3f}"

    return {
        "rps": rps.group(1) if rps else "NA",
        "p50_ms": ms(p50),
        "p99_ms": ms(p99),
        "errors": (errs.group(1).strip() if errs else "") + (f" non2xx={non2xx.group(1)}" if non2xx else ""),
    }


def run_one(name, spec, endpoint, duration, conns, warmup):
    cmd, env, cwd = spec
    e = dict(os.environ, **env)
    p = subprocess.Popen(["taskset", "-c", SERVER_CPUS] + cmd, env=e, cwd=cwd,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    try:
        if not wait_port():
            return {"rps": "NA", "p50_ms": "NA", "p99_ms": "NA", "errors": "start failed", "cpu_pct": "NA",
                    "cores_pct": "NA", "us_per_req": "NA", "rss_mb": "NA"}
        time.sleep(1.0)  # uvicorn の複数ワーカーが揃うまで
        if warmup:
            wrk(PATHS[endpoint], warmup, conns)
        pids = tree_pids(p.pid)
        c0, b0, t0 = cpu_ticks(pids), cores_busy(SERVER_CPUS), time.time()
        res = wrk(PATHS[endpoint], duration, conns)
        c1, b1, t1 = cpu_ticks(pids), cores_busy(SERVER_CPUS), time.time()
        hz = os.sysconf("SC_CLK_TCK")
        res["cpu_pct"] = f"{100 * (c1 - c0) / hz / (t1 - t0):.0f}"
        res["cores_pct"] = f"{100 * (b1 - b0) / hz / (t1 - t0):.0f}"
        # サーバのコアの busy 時間 / 要求数（wrk の計測時間 ≒ t1 - t0）
        res["us_per_req"] = (f"{1e6 * (b1 - b0) / hz / (float(res['rps']) * duration):.1f}"
                             if res["rps"] != "NA" else "NA")
        res["rss_mb"] = f"{rss_mb(tree_pids(p.pid)):.1f}"
        return res
    finally:
        try:
            os.killpg(p.pid, signal.SIGTERM)
            p.wait(timeout=10)
        except Exception:
            os.killpg(p.pid, signal.SIGKILL)
            p.wait()
        time.sleep(0.5)


def summary(path):
    rows = list(csv.DictReader(open(path), delimiter="\t"))
    names = list(dict.fromkeys(r["target"] for r in rows))
    for ep in dict.fromkeys(r["endpoint"] for r in rows):
        print(f"\n#### {ep}\n")
        print("| target | req/s (median) | p50 ms | p99 ms | process CPU % | server cores busy % | CPU µs/req | RSS MB | runs | errors |")
        print("|---|---:|---:|---:|---:|---:|---:|---:|---:|---|")
        for n in names:
            rs = [r for r in rows if r["target"] == n and r["endpoint"] == ep]
            ok = [r for r in rs if r["rps"] != "NA"]
            if not ok:
                print(f"| {n} | NA | | | | | | | {len(rs)} | {rs[0]['errors'] if rs else ''} |")
                continue

            def med(k):
                v = [float(r[k]) for r in ok if r[k] != "NA"]
                return statistics.median(v) if v else float("nan")

            errs = sorted({r["errors"] for r in rs if r["errors"]})
            print(f"| {n} | {med('rps'):,.0f} | {med('p50_ms'):.2f} | {med('p99_ms'):.2f} | {med('cpu_pct'):.0f} | "
                  f"{med('cores_pct'):.0f} | {med('us_per_req'):.1f} | {med('rss_mb'):.1f} | {len(ok)} | {'; '.join(errs)} |")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--group", choices=list(TARGETS))
    ap.add_argument("--only", help="対象名の部分一致（カンマ区切り）")
    ap.add_argument("--rounds", type=int, default=3)
    ap.add_argument("--duration", type=int, default=10)
    ap.add_argument("--warmup", type=int, default=3)
    ap.add_argument("--conns", type=int, default=64)
    ap.add_argument("--endpoints", default="plaintext,json")
    ap.add_argument("--out")
    ap.add_argument("--summary")
    a = ap.parse_args()
    if a.summary:
        summary(a.summary)
        return
    targets = TARGETS[a.group]
    if a.only:
        keys = [k.strip() for k in a.only.split(",")]
        targets = {n: s for n, s in targets.items() if any(k in n for k in keys)}
    names = list(targets)
    cols = ["round", "target", "endpoint", "rps", "p50_ms", "p99_ms", "cpu_pct", "cores_pct", "us_per_req", "rss_mb", "errors"]
    with open(a.out, "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=cols, delimiter="\t")
        w.writeheader()
        for r in range(1, a.rounds + 1):
            # ラウンドごとに順番を回転・反転して、時間の偏りを相殺する
            k = (r - 1) % len(names)
            order = names[k:] + names[:k]
            if r % 2 == 0:
                order.reverse()
            for ep in a.endpoints.split(","):
                for n in order:
                    res = run_one(n, targets[n], ep, a.duration, a.conns, a.warmup)
                    row = {"round": r, "target": n, "endpoint": ep, **res}
                    w.writerow(row)
                    f.flush()
                    print(f"[{time.strftime('%H:%M:%S')}] r{r} {ep:9} {n:34} {res['rps']:>10} rps "
                          f"p99={res['p99_ms']}ms cpu={res['cpu_pct']}% cores={res['cores_pct']}% {res['us_per_req']}us/req {res['errors']}", flush=True)
    summary(a.out)


if __name__ == "__main__":
    sys.exit(main())
