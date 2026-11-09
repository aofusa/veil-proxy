#!/usr/bin/env python3
"""F-300: プロキシ本体の交互 A/B（Docker を使わない退行確認）。手順は tools/perf/README.md「プロキシ本体の交互 A/B」。

    python3 tools/perf/proxy_ab.py --base ~/base/target/release/veil --new target/release/veil --out r.tsv
    python3 tools/perf/proxy_ab.py --base A --new B --cases 'h2c_' --rounds 5 --ktls
    python3 tools/perf/proxy_ab.py --summary r.tsv

`suite_ab.sh` はイメージを 2 つ作って全構成を何時間も回す。こちらは手元の 2 つの `veil` を CPU に固定して、
HTTP/1.1・HTTP/2・h2c・HTTP/3 × 静的配信・上流への転送 × 3B・54KB を 1 ケースごとに交互に測る（ケースごとに
起動し直し、ラウンドごとに順番を入れ替える）。上流は nginx（`--nginx`）。負荷は wrk・h2load、HTTP/3 は
`--h3load` に渡した QUIC 対応の h2load か `tools/perf/h3load`（渡したときだけ測る）。
"""

import argparse
import csv
import os
import re
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from api_ab import cpu_ticks, port_open, wait_port  # noqa: E402

COLUMNS = ["round", "target", "case", "rps", "p99_us", "errors", "wrk_timeouts", "cpu_us_per_req", "user_us_per_req",
           "sys_us_per_req"]
CLEAR, TLS, BACKEND = 18080, 18443, 18081
BODY_54K = 54576


def cases():
    """(名前, 負荷ツール, URL) の一覧。"""
    out = []
    for size, name in (("3b", "3b.html"), ("54k", "index.html")):
        for kind, prefix in (("file", "/"), ("proxy", "/p/")):
            clear = f"http://127.0.0.1:{CLEAR}{prefix}{name}"
            tls = f"https://127.0.0.1:{TLS}{prefix}{name}"
            out += [
                (f"h1_{kind}_{size}", "wrk", clear),
                (f"h1tls_{kind}_{size}", "wrk", tls),
                (f"h2c_{kind}_{size}", "h2load", clear),
                (f"h2_{kind}_{size}", "h2load", tls),
                (f"h3_{kind}_{size}", "h3load", tls),
            ]
    return out


def write_config(path, work, threads, ktls, file_caches):
    # キャッシュ無しの静的配信は、HTTP/2・HTTP/3 が要求ごとに offload する（B-354 の経路）
    static_cache = """
[static_file_cache]
enabled = true
valid_duration_secs = 60
max_entries = 1024
max_file_size_bytes = 1048576
""" if file_caches else ""
    route_cache = """[route.open_file_cache]
enabled = true
valid_duration_secs = 60
max_entries = 1024
""" if file_caches else ""
    path.write_text(f"""[server]
listen = "127.0.0.1:{TLS}"
listen_cleartext = "127.0.0.1:{CLEAR}"
enable_secure_redirect = false
http2_enabled = true
threads = {threads}

[logging]
level = "warn"

[performance]
huge_pages_enabled = false
reuseport_balancing = "kernel"

[tls]
cert_path = "{work}/cert.pem"
key_path = "{work}/key.pem"
ktls_enabled = {str(ktls).lower()}
ktls_fallback_enabled = true

[http3]
enabled = true
listen = "127.0.0.1:{TLS}"
{static_cache}
[upstreams."backend"]
algorithm = "round_robin"
servers = ["http://127.0.0.1:{BACKEND}/"]

[[route]]
[route.conditions]
path = "/p/*"
[route.action]
type = "Proxy"
upstream = "backend"
[route.security]
allowed_methods = ["HEAD", "GET"]

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "{work}/www/"
[route.security]
allowed_methods = ["HEAD", "GET"]
{route_cache}""")


def write_nginx(path, work, workers):
    path.write_text(f"""worker_processes {workers};
daemon off;
pid {work}/nginx.pid;
error_log /dev/null;
events {{ worker_connections 8192; }}
http {{
    access_log off;
    sendfile on;
    tcp_nopush on;
    keepalive_timeout 65;
    keepalive_requests 1000000;
    client_body_temp_path {work}/nginx-tmp;
    proxy_temp_path {work}/nginx-tmp;
    fastcgi_temp_path {work}/nginx-tmp;
    uwsgi_temp_path {work}/nginx-tmp;
    scgi_temp_path {work}/nginx-tmp;
    server {{
        listen 127.0.0.1:{BACKEND} reuseport;
        root {work}/up;
    }}
}}
""")


def prepare(work):
    www, up = work / "www", work / "up"
    www.mkdir()
    (up / "p").mkdir(parents=True)
    (work / "nginx-tmp").mkdir()
    # 静的配信と上流で根を分ける（ルートを間違えると 404 で errs に出る）。上流は接頭辞の有無のどちらでも返す
    for d in (www, up, up / "p"):
        (d / "3b.html").write_bytes(b"abc")
        (d / "index.html").write_bytes(b"x" * BODY_54K)
    subprocess.run(
        ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", work / "key.pem",
         "-out", work / "cert.pem", "-days", "2", "-subj", "/CN=localhost"],
        check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )


def parse_wrk(out):
    rps = float(re.search(r"Requests/sec:\s+([0-9.]+)", out).group(1))
    p99 = 0.0
    m = re.search(r"^\s*99%\s+([0-9.]+)(us|ms|s)\s*$", out, re.M)
    if m:
        p99 = float(m.group(1)) * {"us": 1, "ms": 1000, "s": 1000000}[m.group(2)]
    errs = sum(int(x) for x in re.findall(r"(?:connect|read|write) (\d+)", out))
    # wrk の timeout は gettimeofday の差。時計が戻るとその瞬間に処理中の全接続を数える（WSL2 は 30 秒ごとに約 75 ms 戻る）
    m = re.search(r"timeout (\d+)", out)
    timeouts = int(m.group(1)) if m else 0
    m = re.search(r"Non-2xx or 3xx responses: (\d+)", out)
    return rps, p99, errs + (int(m.group(1)) if m else 0), timeouts


def parse_h2load(out):
    rps = float(re.search(r"finished in [^,]+, ([0-9.]+) req/s", out).group(1))
    m = re.search(r"(\d+) failed, (\d+) errored, (\d+) timeout", out)
    errs = sum(int(x) for x in m.groups()) if m else 0
    m = re.search(r"status codes: \d+ 2xx, (\d+) 3xx, (\d+) 4xx, (\d+) 5xx", out)
    return rps, 0.0, errs + (sum(int(x) for x in m.groups()) if m else 0), 0


def load_cmd(args, name, tool, url, secs):
    # 転送の行は上流に CPU を渡すぶん負荷の CPU を減らす（静的配信の行は上流が眠っているので 12・13 も使える）
    proxied = "_proxy_" in name
    pin = ["taskset", "-c", args.proxy_load_cpus if proxied else args.load_cpus]
    t = str(args.proxy_load_threads if proxied else args.load_threads)
    if tool == "wrk":
        return pin + ["wrk", "-t", t, "-c", str(args.conns), "-d", f"{secs}s", "--latency", url], parse_wrk
    if tool == "h2load":
        return pin + ["h2load", "-t", t, "-c", str(args.conns), "-m", str(args.streams), "-D", str(secs), url], parse_h2load
    if Path(args.h3load).name.startswith("h2load"):
        return pin + [args.h3load, "--alpn-list=h3", "-t", t, "-c", str(args.conns), "-m", str(args.streams), "-D", str(secs),
                      url], parse_h2load
    return pin + [args.h3load, "-t", t, "-c", str(args.conns), "-m", str(args.streams), "-d", str(secs), url], parse_h2load


def run_load(cmd, parse):
    r = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    try:
        return parse(r.stdout)
    except AttributeError:
        raise SystemExit(f"could not parse the load tool output:\n{r.stdout[-1500:]}")


def stop(proc):
    proc.send_signal(signal.SIGTERM)
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()


def measure(args, binary, config, target, rnd, case, writer):
    name, tool, url = case
    addr = f"127.0.0.1:{CLEAR}"
    if port_open(addr):
        raise SystemExit(f"{addr} is already in use")  # 残ったサーバを測らない（B-218）
    srv = subprocess.Popen(
        ["taskset", "-c", args.server_cpus, binary, "-c", str(config)],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        wait_port(addr, srv)
        time.sleep(0.3)
        cmd, parse = load_cmd(args, name, tool, url, 1)
        run_load(cmd, parse)  # 暖機（キャッシュと上流のプールを埋める）
        cmd, parse = load_cmd(args, name, tool, url, args.duration)
        u0, s0 = cpu_ticks(srv.pid)
        rps, p99, errs, timeouts = run_load(cmd, parse)
        u1, s1 = cpu_ticks(srv.pid)
    finally:
        stop(srv)
    n = max(rps * args.duration, 1.0)
    row = [rnd, target, name, f"{rps:.0f}", f"{p99:.0f}", errs, timeouts, f"{(u1 - u0 + s1 - s0) * 1e6 / n:.2f}",
           f"{(u1 - u0) * 1e6 / n:.2f}", f"{(s1 - s0) * 1e6 / n:.2f}"]
    writer.writerow(row)
    print("\t".join(str(x) for x in row), flush=True)


def summarize(path):
    with open(path) as f:
        rows = list(csv.DictReader(f, delimiter="\t"))
    names = list(dict.fromkeys(r["case"] for r in rows))
    print(f"{'case':<20}{'base rps':>12}{'new rps':>12}{'new/base':>10}{'wins':>7}{'base cpu':>10}{'new cpu':>9}{'errs':>6}")
    for name in names:
        col = {}
        for t in ("base", "new"):
            col[t] = [r for r in rows if r["case"] == name and r["target"] == t]
        if not col["base"] or not col["new"]:
            continue
        med = {t: statistics.median(float(r["rps"]) for r in col[t]) for t in col}
        cpu = {t: statistics.median(float(r["cpu_us_per_req"]) for r in col[t]) for t in col}
        wins = sum(float(b["rps"]) < float(n["rps"]) for b, n in zip(col["base"], col["new"]))
        errs = sum(int(r["errors"]) for t in col for r in col[t])
        ratio = med["new"] / med["base"] if med["base"] else 0.0
        print(f"{name:<20}{med['base']:>12.0f}{med['new']:>12.0f}{ratio:>10.3f}{wins:>4}/{len(col['new'])}"
              f"{cpu['base']:>10.2f}{cpu['new']:>9.2f}{errs:>6}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--base", help="the baseline veil binary")
    ap.add_argument("--new", help="the veil binary under test")
    ap.add_argument("--out", default="proxy_ab.tsv")
    ap.add_argument("--summary", help="print the medians of a TSV and exit")
    ap.add_argument("--cases", default=".", help="regular expression over the case names")
    ap.add_argument("--rounds", type=int, default=3)
    ap.add_argument("--duration", type=int, default=5)
    ap.add_argument("--conns", type=int, default=128)
    ap.add_argument("--streams", type=int, default=10)
    ap.add_argument("--threads", type=int, default=8, help="veil worker threads")
    ap.add_argument("--server-cpus", default="0-7")
    ap.add_argument("--load-cpus", default="8-13")
    ap.add_argument("--load-threads", type=int, default=6)
    ap.add_argument("--proxy-load-cpus", default="8-11", help="load CPUs of the proxied cases")
    ap.add_argument("--proxy-load-threads", type=int, default=4)
    ap.add_argument("--backend-cpus", default="12-15")
    ap.add_argument("--backend-workers", type=int, default=4)
    ap.add_argument("--nginx", default=shutil.which("nginx") or "/usr/sbin/nginx")
    ap.add_argument("--h3load", help="a QUIC h2load or the tools/perf/h3load binary; without it the HTTP/3 cases are skipped")
    ap.add_argument("--ktls", action="store_true", help="turn kTLS on for both sides")
    ap.add_argument("--no-file-caches", action="store_true",
                    help="serve the files without open_file_cache and the content cache (HTTP/2 and HTTP/3 offload every request)")
    args = ap.parse_args()
    if args.summary:
        summarize(args.summary)
        return
    if not args.base or not args.new:
        ap.error("--base and --new are required")

    selected = [c for c in cases() if re.search(args.cases, c[0]) and (c[1] != "h3load" or args.h3load)]
    # 前の実行の nginx が残っていると、こちらの起動は bind で失敗しても wait_port は残りの応答で通り、`--backend-*` の効かない
    # 別の上流を測る（B-218 のサーバと同じ）
    if port_open(f"127.0.0.1:{BACKEND}"):
        raise SystemExit(f"127.0.0.1:{BACKEND} is already in use")
    # kill で止められたときも finally で上流を止める
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    with tempfile.TemporaryDirectory(prefix="veil-proxy-ab-") as tmp:
        work = Path(tmp)
        prepare(work)
        config = work / "config.toml"
        write_config(config, work, args.threads, args.ktls, not args.no_file_caches)
        nginx_conf = work / "nginx.conf"
        write_nginx(nginx_conf, work, args.backend_workers)
        backend = subprocess.Popen(
            ["taskset", "-c", args.backend_cpus, args.nginx, "-p", str(work), "-c", str(nginx_conf)],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        try:
            wait_port(f"127.0.0.1:{BACKEND}", backend)
            with open(args.out, "w", newline="") as f:
                writer = csv.writer(f, delimiter="\t", lineterminator="\n")
                writer.writerow(COLUMNS)
                for rnd in range(1, args.rounds + 1):
                    for i, case in enumerate(selected):
                        order = [("base", args.base), ("new", args.new)]
                        if (rnd + i) % 2:
                            order.reverse()
                        for target, binary in order:
                            measure(args, os.path.abspath(binary), config, target, rnd, case, writer)
                            f.flush()
        finally:
            stop(backend)
    summarize(args.out)


if __name__ == "__main__":
    main()
