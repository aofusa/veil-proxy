#!/usr/bin/env python3
"""tokio 自身のテスト（tokio 1.53.2 の tests/*.rs）を veil の tokio 互換レイヤーで実行して合格率を出す（F-198）。

    python3 tools/tokio-compat/run_tokio_tests.py [--epoll] [--filter REGEX] [--timeout SECS] [--out FILE]

- tokio のソースは cargo のレジストリ（~/.cargo/registry/src）から取る。無ければ crates.io から取得する。
- target/tokio-compat-tests/ にテスト用のクレートを作り、`[patch.crates-io] tokio = crates/veil-tokio/tokio-shim`
  で tokio を置き換えて `cargo test --no-run --keep-going` でビルドする（ビルドできないファイルは全件失敗として数える）。
- テストは 1 件ずつ別プロセス・タイムアウト付きで実行する（固まったテストが他を巻き込まないように）。
- 分類（ファイル名の接頭辞）ごとの合格率と、対象外にしたファイルとその理由を Markdown で出す。
  受け入れ条件（F-198）は time・io・net・sync の各分類で 9 割以上。
"""
import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import urllib.request
from collections import defaultdict
from pathlib import Path

VERSION = "1.53.2"
ROOT = Path(__file__).resolve().parents[2]
WORK = ROOT / "target" / "tokio-compat-tests"

# 対象外（分母に入れない）: tokio のランタイム内部・未対応の機能・他 OS 向け
EXCLUDE = [
    (r"^rt_", "ランタイム内部の挙動（work stealing・メトリクス・ワーカー番号・LIFO スロット・コールバック）"),
    (r"^process_", "tokio::process は未対応（tokio-compat-bridge を使う）"),
    (r"^fs_uring", "tokio 自身の io-uring ファイル I/O（tokio_unstable）"),
    (r"^(tracing_|task_trace_|dump$|task_hooks$)", "tokio_unstable / taskdump / トレースのフック"),
    (r"^(time_pause|test_clock|time_alt)$", "時計の停止（test-util の pause/advance）は未対応"),
    (r"(_windows|^net_named_pipe)$", "Windows 専用"),
    (r"^net_unix_pipe$", "tokio::net::unix::pipe と tokio::process（子プロセス）は未対応"),
    (r"^(coop_budget|task_builder|async_send_sync|unwindsafe|_require_full|no_rt|time_wasm|io_poll_aio)$",
     "協調スケジューリングの予算・tokio_unstable の Builder・型の自動トレイトの網羅検査・特定 OS 専用"),
]

CATEGORIES = [
    ("time", r"^time_"),
    ("io", r"^(io_|buffered$|duplex_stream$)"),
    ("net", r"^(net_|tcp_|udp$|uds_)"),
    ("sync", r"^sync_"),
    ("task", r"^(task_|join_handle_panic$)"),
    ("macros", r"^macros_"),
    ("fs", r"^fs"),
    ("signal", r"^signal_"),
]


def category(name):
    for c, rx in CATEGORIES:
        if re.search(rx, name):
            return c
    return "other"


def excluded(name):
    for rx, why in EXCLUDE:
        if re.search(rx, name):
            return why
    return None


def tokio_source():
    reg = Path.home() / ".cargo" / "registry" / "src"
    for d in reg.glob(f"*/tokio-{VERSION}"):
        if (d / "tests").is_dir():
            return d
    dst = WORK / f"tokio-{VERSION}"
    if not dst.exists():
        WORK.mkdir(parents=True, exist_ok=True)
        url = f"https://crates.io/api/v1/crates/tokio/{VERSION}/download"
        tgz = WORK / "tokio.tar.gz"
        urllib.request.urlretrieve(url, tgz)
        with tarfile.open(tgz) as t:
            t.extractall(WORK, filter="data")
    return dst


def make_crate(src, epoll):
    crate = WORK / "crate"
    if crate.exists():
        shutil.rmtree(crate / "tests", ignore_errors=True)
    (crate / "src").mkdir(parents=True, exist_ok=True)
    shutil.copytree(src / "tests", crate / "tests")
    (crate / "src" / "lib.rs").write_text("// tokio のテストを動かすための空のクレート\n")
    veil_features = '["tokio-compat", "epoll"]' if epoll else '["tokio-compat"]'
    (crate / "Cargo.toml").write_text(f'''[package]
name = "tokio-tests-on-veil"
version = "0.1.0"
edition = "2021"
publish = false

[workspace]

[features]
default = ["full"]
full = ["fs", "io-util", "io-std", "macros", "net", "process", "rt", "rt-multi-thread", "signal", "sync", "time"]
fs = []
io-util = []
io-std = []
macros = []
net = []
process = []
rt = []
rt-multi-thread = []
signal = []
sync = []
time = []
parking_lot = []

[dependencies]
tokio = {{ version = "1", features = ["full"] }}
veil = {{ path = "{ROOT}", default-features = false, features = {veil_features} }}

[dev-dependencies]
async-stream = "0.3"
bytes = "1"
pin-project-lite = "0.2"
futures = {{ version = "0.3", features = ["async-await"] }}
futures-concurrency = "7.6.3"
futures-test = "0.3.31"
mockall = "0.13"
tokio-stream = "0.1"
tokio-test = "0.4"
tokio-util = {{ version = "0.7", features = ["rt"] }}
socket2 = "0.6"
libc = "0.2"
nix = {{ version = "0.30", default-features = false, features = ["fs", "socket", "signal", "process"] }}
rand = "0.9"
tempfile = "3"

[patch.crates-io]
tokio = {{ path = "{ROOT}/crates/veil-tokio/tokio-shim" }}
quiche = {{ path = "{ROOT}/third_party/quiche" }}
''')
    return crate


def build(crate):
    env = dict(os.environ, CARGO_TARGET_DIR=str(WORK / "target"))
    p = subprocess.run(
        ["cargo", "build", "--tests", "--keep-going", "--message-format=json"],
        cwd=crate, env=env, capture_output=True, text=True,
    )
    exes, failed = {}, set()
    for line in p.stdout.splitlines():
        try:
            m = json.loads(line)
        except ValueError:
            continue
        if m.get("reason") == "compiler-artifact" and m.get("executable") and "test" in m["target"]["kind"]:
            exes[m["target"]["name"]] = m["executable"]
        if m.get("reason") == "compiler-message" and m["message"].get("level") == "error":
            t = m.get("target", {}).get("name")
            if t:
                failed.add(t)
    errs = [l for l in p.stderr.splitlines() if l.startswith("error")]
    return exes, failed, errs


def count_tests(path):
    text = path.read_text(errors="replace")
    return len(re.findall(r"#\[(tokio::test|test|maybe_tokio_test)\b", text))


def run_one(exe, test, timeout):
    try:
        p = subprocess.run([exe, "--exact", test, "--test-threads=1"], capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired:
        return "timeout", ""
    out = p.stdout + p.stderr
    if "1 passed" in out:
        return "ok", out
    if "1 ignored" in out or "0 passed; 0 failed; 1 ignored" in out:
        return "ignored", out
    return "fail", out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--epoll", action="store_true")
    ap.add_argument("--filter", default="")
    ap.add_argument("--timeout", type=int, default=60)
    ap.add_argument("--out", default=str(ROOT / "docs" / "artifacts" / "tokio_compat_results.md"))
    a = ap.parse_args()

    src = tokio_source()
    crate = make_crate(src, a.epoll)
    print(f"building tokio {VERSION} tests against veil::tokio ({'epoll' if a.epoll else 'io_uring'}) ...", flush=True)
    exes, build_failed, errs = build(crate)

    per_file = {}
    failures = defaultdict(list)
    for f in sorted((crate / "tests").glob("*.rs")):
        name = f.stem
        if a.filter and not re.search(a.filter, name):
            continue
        why = excluded(name)
        exe = exes.get(name)
        if exe is None:
            n = count_tests(f)
            per_file[name] = {"ok": 0, "fail": n, "timeout": 0, "ignored": 0, "build": "failed", "excluded": why}
            continue
        lst = subprocess.run([exe, "--list", "--format", "terse"], capture_output=True, text=True).stdout
        tests = [l[: -len(": test")] for l in lst.splitlines() if l.endswith(": test")]
        r = {"ok": 0, "fail": 0, "timeout": 0, "ignored": 0, "build": "ok", "excluded": why}
        for t in tests:
            st, out = run_one(exe, t, a.timeout)
            r[st] += 1
            if st in ("fail", "timeout") and not why:
                tail = [l for l in out.splitlines() if "panicked" in l or "assert" in l][:2]
                failures[name].append((t, st, " / ".join(tail)[:300]))
        per_file[name] = r
        print(f"{name:32} ok={r['ok']:3} fail={r['fail']:3} timeout={r['timeout']:2} {'(excluded)' if why else ''}", flush=True)

    cats = defaultdict(lambda: [0, 0])
    for name, r in per_file.items():
        if r["excluded"]:
            continue
        c = category(name)
        cats[c][0] += r["ok"]
        cats[c][1] += r["ok"] + r["fail"] + r["timeout"]

    lines = [f"# tokio {VERSION} のテストを veil::tokio で実行した結果（{'epoll' if a.epoll else 'io_uring'}）", ""]
    lines += ["| 分類 | 合格 / 対象 | 合格率 |", "|------|------------|--------|"]
    for c, _ in CATEGORIES + [("other", "")]:
        ok, n = cats.get(c, [0, 0])
        if n:
            lines.append(f"| {c} | {ok} / {n} | {100 * ok / n:.1f}% |")
    tot_ok = sum(v[0] for v in cats.values())
    tot = sum(v[1] for v in cats.values())
    lines.append(f"| **全体** | {tot_ok} / {tot} | {100 * tot_ok / max(tot, 1):.1f}% |")
    lines += ["", "## ファイルごと", "", "| ファイル | 分類 | 合格 | 失敗 | タイムアウト | 備考 |", "|---|---|---|---|---|---|"]
    for name, r in sorted(per_file.items()):
        note = f"対象外: {r['excluded']}" if r["excluded"] else ("ビルド失敗" if r["build"] == "failed" else "")
        lines.append(f"| {name} | {category(name)} | {r['ok']} | {r['fail']} | {r['timeout']} | {note} |")
    lines += ["", "## 対象外", ""]
    for rx, why in EXCLUDE:
        lines.append(f"- `{rx}`: {why}")
    lines += ["", "## 失敗したテスト（対象のもの）", ""]
    for name, fs in sorted(failures.items()):
        for t, st, msg in fs:
            lines.append(f"- {name}::{t} ({st}) {msg}")
    if errs:
        lines += ["", "## ビルドエラー（先頭）", "", "```"] + errs[:40] + ["```"]
    Path(a.out).parent.mkdir(parents=True, exist_ok=True)
    Path(a.out).write_text("\n".join(lines) + "\n")
    print("\n".join(lines[:16]))
    print(f"\nwritten: {a.out}")


if __name__ == "__main__":
    main()
