#!/usr/bin/env python3
"""F-295: API 経路の交互 A/B（json-comp・コア制限の遅延・gRPC・Python）。手順は tools/perf/README.md「API 経路の交互 A/B」。

    python3 tools/perf/api_ab.py --scenario json --new . --base ~/base --rounds 5 --out r.tsv
    python3 tools/perf/api_ab.py --scenario latency --new . --rates 200000,300000,400000
    python3 tools/perf/api_ab.py --scenario python --new . --new-venv v1 --base ~/base --base-venv v0
    python3 tools/perf/api_ab.py --summary r.tsv

対象は作業ツリー（`--new`・`--base`）。`tools/perf/api-app/target/release/veil-api-app` を実行する（`--app-subpath` で
別のビルドを指す）。負荷は `--new` の `tools/perf/veil-load/target/release/veil-load`。ラウンドごとに対象の順を入れ替える。
"""

import argparse
import os
import re
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

COLUMNS = [
    "round", "target", "row", "rps", "mean_us", "p50_us", "p90_us", "p99_us", "p999_us", "max_us", "rate_ratio",
    "errors", "non2xx", "bytes_per_resp", "cpu_us_per_req", "user_us_per_req", "sys_us_per_req", "syscalls",
]
METRICS = ["rps", "p50_us", "p99_us", "p999_us", "max_us", "rate_ratio", "bytes_per_resp", "cpu_us_per_req"]
CLK = os.sysconf("SC_CLK_TCK")
GRPC_BODY = bytes.fromhex("000000000408011002")  # SumRequest{a=1,b=2}
JSON_PATHS = ["/json/25?m=4", "/json/40?m=8", "/json/50?m=6"]
GRPC_PATH = "/benchmark.BenchmarkService/GetSum"


def cpu_ticks(pid):
    """プロセス全体の utime・stime（秒）。"""
    with open(f"/proc/{pid}/stat") as f:
        fields = f.read().rsplit(")", 1)[1].split()
    return int(fields[11]) / CLK, int(fields[12]) / CLK


def port_open(addr):
    host, port = addr.rsplit(":", 1)
    with socket.socket() as s:
        s.settimeout(0.2)
        return s.connect_ex((host, int(port))) == 0


def wait_port(addr, proc, secs=30):
    end = time.time() + secs
    while time.time() < end:
        if proc.poll() is not None:
            raise SystemExit(f"server exited with {proc.returncode}")
        if port_open(addr):
            return
        time.sleep(0.05)
    raise SystemExit("server did not start listening")


def parse_load(out):
    """veil-load の 1 行（k=v）を辞書に。"""
    return {k: float(v) for k, v in re.findall(r"(\w+)=([0-9.]+)", out.strip().splitlines()[-1])}


class Server:
    """起動中のサーバ（CPU に固定）。"""

    def __init__(self, cmd, env, cpus, addr, count_syscalls=None):
        if port_open(addr):
            raise SystemExit(f"{addr} is already in use")  # 残ったサーバを測らない（B-218）
        full_env = dict(os.environ, **env)
        self.sc_out = None
        if count_syscalls:
            self.sc_out = tempfile.mktemp(prefix="veil-sc-")
            full_env.update(LD_PRELOAD=count_syscalls, SC_OUT=self.sc_out)
        log = open(os.environ.get("API_AB_SERVER_LOG", "/dev/null"), "ab")
        self.proc = subprocess.Popen(["taskset", "-c", cpus] + cmd, env=full_env, stdout=log, stderr=log)
        wait_port(addr, self.proc)

    def stop(self):
        self.proc.send_signal(signal.SIGTERM)
        try:
            self.proc.wait(timeout=8)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()

    def syscalls(self, requests):
        if not self.sc_out:
            return ""
        self.proc.send_signal(signal.SIGUSR2)
        time.sleep(0.3)
        try:
            with open(self.sc_out) as f:
                pairs = [line.split() for line in f]
            os.unlink(self.sc_out)
        except OSError:
            return ""
        return " ".join(f"{k}={int(v) / requests:.3f}" for k, v in pairs if int(v) / requests >= 0.0005)


def run_row(args, srv, target, rnd, row, load_args, writer, warmup=None):
    """1 行の計測。CPU は負荷の間だけの増分を要求数で割る。"""
    u0, s0 = cpu_ticks(srv.proc.pid)
    warm = args.warmup if warmup is None else warmup
    cmd = [args.load, "--target", args.addr, "--pin", args.load_cpus, "-t", str(args.load_threads), "-d", str(args.duration),
           "--warmup", str(warm)] + load_args
    r = subprocess.run(cmd, capture_output=True, text=True, timeout=args.duration + warm + 60)
    if r.returncode != 0:
        print(r.stderr, file=sys.stderr)
        raise SystemExit(f"veil-load failed: {row}")
    m = parse_load(r.stdout)
    u1, s1 = cpu_ticks(srv.proc.pid)
    # veil-load は計測時間の分だけの rps を返す。CPU はウォームアップを含むのでその分の要求を足して割る
    reqs = max(m.get("rps", 0) * (args.duration + warm), 1)
    user, sys_ = (u1 - u0) * 1e6 / reqs, (s1 - s0) * 1e6 / reqs
    vals = dict(m, round=rnd, target=target, row=row, cpu_us_per_req=f"{user + sys_:.2f}", user_us_per_req=f"{user:.2f}",
                sys_us_per_req=f"{sys_:.2f}", syscalls=srv.syscalls(reqs) if args.syscalls and rnd == 0 else "")
    writer(vals)
    print(f"  {target:8} {row:24} rps={m.get('rps', 0):>10.0f} p50={m.get('p50_us', 0):>9.1f} p99={m.get('p99_us', 0):>11.1f} "
          f"p99.9={m.get('p999_us', 0):>11.1f} max={m.get('max_us', 0):>11.1f} ratio={m.get('rate_ratio', 0):.3f} "
          f"err={m.get('errors', 0):.0f} cpu={user + sys_:.1f}us {vals['syscalls']}", flush=True)
    return m


def app_cmd(tree, args):
    return [str(Path(tree) / args.app_subpath), args.addr, str(args.threads)]


def scenario_json(args, trees, rnd, writer):
    paths = sum((["--path", p] for p in JSON_PATHS), [])
    for name, tree in trees:
        # 識別（直列化 + 送信）は圧縮を切った構成と同じ。増分が圧縮のコスト
        srv = Server(app_cmd(tree, args), {"BENCH_COMPRESSION": "1"}, args.server_cpus, args.addr, args.sc_so)
        try:
            run_row(args, srv, name, rnd, "identity", ["-c", str(args.conns)] + paths, writer)
            run_row(args, srv, name, rnd, "gzip", ["-c", str(args.conns), "-H", "Accept-Encoding: gzip"] + paths, writer)
            run_row(args, srv, name, rnd, "br", ["-c", str(args.conns), "-H", "Accept-Encoding: gzip, br"] + paths, writer)
        finally:
            srv.stop()


def scenario_latency(args, trees, rnd, writer):
    path = ["--path", "/baseline11?a=1&b=2", "-c", str(args.conns)]
    for name, tree in trees:
        srv = Server(app_cmd(tree, args), {}, args.server_cpus, args.addr, args.sc_so)
        try:
            run_row(args, srv, name, rnd, "saturation", path, writer)
            for r in args.rates:
                run_row(args, srv, name, rnd, f"rate-{r // 1000}k", path + ["--rate", str(r)], writer)
        finally:
            srv.stop()


def scenario_grpc(args, trees, rnd, writer):
    body = tempfile.NamedTemporaryFile(suffix=".bin", delete=False)
    body.write(GRPC_BODY)
    body.close()
    grpc = ["--mode", "grpc", "--path", GRPC_PATH, "--body-file", body.name]
    try:
        for name, tree in trees:
            srv = Server(app_cmd(tree, args), {}, args.server_cpus, args.addr, args.sc_so)
            try:
                run_row(args, srv, name, rnd, "grpc-c256-m100", grpc + ["-c", "256", "-m", "100"], writer)
                run_row(args, srv, name, rnd, "grpc-c64-m10", grpc + ["-c", "64", "-m", "10"], writer)
                run_row(args, srv, name, rnd, "grpc-c256-m1", grpc + ["-c", "256", "-m", "1"], writer)
                run_row(args, srv, name, rnd, "h2-c256-m100",
                        ["--mode", "h2", "--path", "/baseline11?a=1&b=2", "-c", "256", "-m", "100"], writer)
            finally:
                srv.stop()
    finally:
        os.unlink(body.name)


def scenario_python(args, trees, rnd, writer):
    variants = {  # 名前 → 環境変数
        "offload": {"BENCH_OFFLOAD": "1", "BENCH_ASYNC": "0"},
        "inline": {"BENCH_OFFLOAD": "0", "BENCH_ASYNC": "0"},
        "async": {"BENCH_OFFLOAD": "0", "BENCH_ASYNC": "1"},
    }
    for name, tree, venv in trees:
        app = Path(tree) / "tools/api-bench/python/veil_app.py"
        for v in args.py_variants:
            for threads in args.py_threads:
                env = dict(variants[v], BENCH_THREADS=str(threads), BENCH_LISTEN=args.addr)
                if args.no_access_log:
                    env["BENCH_ACCESS_LOG"] = "0"
                srv = Server([str(Path(venv) / "bin/python"), str(app)], env, args.server_cpus, args.addr, args.sc_so)
                try:
                    c = ["-c", str(args.conns)]
                    run_row(args, srv, name, rnd, f"{v}-t{threads}-plaintext", c + ["--path", "/plaintext"], writer)
                    run_row(args, srv, name, rnd, f"{v}-t{threads}-json", c + ["--path", "/json/abc"], writer)
                finally:
                    srv.stop()


def summarize(path):
    with open(path) as f:
        header = f.readline().rstrip("\n").split("\t")
        rows = [dict(zip(header, line.rstrip("\n").split("\t"))) for line in f if line.strip()]
    targets = list(dict.fromkeys(r["target"] for r in rows))
    table = {}
    for r in rows:
        table.setdefault(r["row"], {}).setdefault(r["target"], []).append(r)
    shown = [m for m in METRICS]
    print("row\ttarget\tn\t" + "\t".join(shown) + ("\tvs_base_rps\tvs_base_cpu" if len(targets) > 1 else ""))
    for row, by_target in table.items():
        med = {}
        for t in targets:
            rs = by_target.get(t, [])
            if not rs:
                continue
            med[t] = {m: statistics.median(float(r[m] or 0) for r in rs) for m in shown}
            line = [row, t, str(len(rs))] + [f"{med[t][m]:.2f}" if m in ("rate_ratio", "cpu_us_per_req") else f"{med[t][m]:.0f}" for m in shown]
            if len(targets) > 1 and t != targets[-1] and targets[-1] in by_target:
                base = {m: statistics.median(float(r[m] or 0) for r in by_target[targets[-1]]) for m in shown}
                line.append(f"{med[t]['rps'] / base['rps']:.2f}x" if base["rps"] else "-")
                line.append(f"{med[t]['cpu_us_per_req'] / base['cpu_us_per_req']:.2f}x" if base["cpu_us_per_req"] else "-")
            print("\t".join(line))


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--scenario", choices=["json", "latency", "grpc", "python", "all"], default="all")
    p.add_argument("--new", default=".", help="tree under test")
    p.add_argument("--base", help="baseline tree (omit to measure one tree)")
    p.add_argument("--new-venv", help="venv with the wheel of --new (python scenario)")
    p.add_argument("--base-venv", help="venv with the wheel of --base (python scenario)")
    p.add_argument("--app-subpath", default="tools/perf/api-app/target/release/veil-api-app")
    p.add_argument("--load", help="veil-load binary (default: the one in --new)")
    p.add_argument("--addr", default="127.0.0.1:3000")
    p.add_argument("--server-cpus", default="0-7")
    p.add_argument("--load-cpus", default="8-15")
    p.add_argument("--threads", type=int, default=8, help="server worker threads")
    p.add_argument("--load-threads", type=int, default=8)
    p.add_argument("--conns", type=int, help="connections (default: json 256, latency 1024, python 64)")
    p.add_argument("--duration", type=int, default=6)
    p.add_argument("--warmup", type=int, default=1)
    p.add_argument("--rounds", type=int, default=3)
    p.add_argument("--rates", default="200000,300000,400000", help="fixed rates of the latency scenario")
    p.add_argument("--py-variants", default="offload,inline,async")
    p.add_argument("--py-threads", default="2,4")
    p.add_argument("--no-access-log", action="store_true", help="python scenario: turn the access log off")
    p.add_argument("--syscalls", action="store_true", help="count syscalls per request in round 0 (LD_PRELOAD)")
    p.add_argument("--out", default="api_ab.tsv")
    p.add_argument("--summary", help="print the median table of an existing TSV and exit")
    args = p.parse_args()
    if args.summary:
        summarize(args.summary)
        return
    args.rates = [int(x) for x in args.rates.split(",") if x]
    args.py_variants = [x for x in args.py_variants.split(",") if x]
    args.py_threads = [int(x) for x in args.py_threads.split(",") if x]
    args.load = args.load or str(Path(args.new) / "tools/perf/veil-load/target/release/veil-load")
    args.sc_so = None
    if args.syscalls:
        cc = shutil.which("cc")
        if not cc:
            raise SystemExit("--syscalls needs cc")
        args.sc_so = str(Path(tempfile.mkdtemp()) / "sc.so")
        src = Path(__file__).resolve().parent / "syscall_count/sc.c"
        subprocess.run([cc, "-O2", "-shared", "-fPIC", "-o", args.sc_so, str(src), "-ldl"], check=True)
    trees = [("new", args.new)] + ([("base", args.base)] if args.base else [])
    py_trees = [("new", args.new, args.new_venv)] + ([("base", args.base, args.base_venv)] if args.base else [])
    out = open(args.out, "w")
    out.write("\t".join(COLUMNS) + "\n")

    def writer(vals):
        out.write("\t".join(str(vals.get(c, "")) for c in COLUMNS) + "\n")
        out.flush()

    scenarios = ["json", "latency", "grpc", "python"] if args.scenario == "all" else [args.scenario]
    conns_given = args.conns
    for rnd in range(args.rounds):
        # ラウンドごとに順を入れ替える（先に測る側の有利・不利を打ち消す）
        order = trees if rnd % 2 == 0 else list(reversed(trees))
        py_order = py_trees if rnd % 2 == 0 else list(reversed(py_trees))
        for s in scenarios:
            print(f"[round {rnd}] {s}", flush=True)
            if s == "json":
                args.conns = conns_given or 256
                scenario_json(args, order, rnd, writer)
            elif s == "latency":
                args.conns = conns_given or 1024
                scenario_latency(args, order, rnd, writer)
            elif s == "grpc":
                scenario_grpc(args, order, rnd, writer)
            else:
                args.conns = conns_given or 64
                scenario_python(args, py_order, rnd, writer)
    out.close()
    summarize(args.out)


if __name__ == "__main__":
    main()
