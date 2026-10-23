#!/usr/bin/env python3
"""F-255: 文書の中の完全な config.toml の例が `veil -t` を通ることを確かめる。

    python3 tools/lint/check-guide-configs.py <veil-binary> <file.md>...

```toml のブロックを取り出して 1 つずつ検査する。設定の断片（そのままでは完全な設定にならないもの）は、直前の行に
`<!-- not-a-full-config -->` を書いて除く。-t は File のルートや証明書が在ることも見るので、使い捨てのコンテナ（debian）の中で、
例が参照するディレクトリ・ファイル・自己署名の証明書を作ってから検査する。
"""
import os
import re
import subprocess
import sys
import tempfile
import tomllib

MARK = "<!-- not-a-full-config -->"


def blocks(path):
    lines = open(path, encoding="utf-8").read().split("\n")
    out = []
    i = 0
    while i < len(lines):
        if lines[i].strip() == "```toml":
            skip = i > 0 and lines[i - 1].strip() == MARK
            j = i + 1
            while j < len(lines) and lines[j].strip() != "```":
                j += 1
            if not skip:
                out.append((i + 1, "\n".join(lines[i + 1:j]) + "\n"))
            i = j
        i += 1
    return out


def needed_paths(cfg):
    dirs, files, certs = set(), set(), set()
    for r in cfg.get("route", []):
        a = r.get("action", {})
        if a.get("type", "File") == "File" and "path" in a:
            p = a["path"]
            (files if re.search(r"\.[a-z0-9]+$", p) else dirs).add(p)
    tls = cfg.get("tls", {})
    for k in ("cert_path", "key_path"):
        if k in tls:
            certs.add((tls.get("cert_path"), tls.get("key_path")))
    return dirs, files, certs


def main():
    binary = os.path.abspath(sys.argv[1])
    work = tempfile.mkdtemp(prefix="veil-guide-", dir=os.environ.get("VEIL_DOCKER_TMP", os.path.expanduser("~")))
    script = ["set -u", "apt-get update -qq >/dev/null && apt-get install -y -qq openssl >/dev/null 2>&1", "fail=0"]
    n = 0
    for md in sys.argv[2:]:
        for line, text in blocks(md):
            n += 1
            name = f"c{n}.toml"
            try:
                cfg = tomllib.loads(text)
            except tomllib.TOMLDecodeError as e:
                print(f"FAIL {md}:{line}: TOML: {e}")
                script.append("fail=1")
                continue
            open(os.path.join(work, name), "w").write(text)
            dirs, files, certs = needed_paths(cfg)
            for d in sorted(dirs):
                script.append(f"mkdir -p '{d}'")
            for f in sorted(files):
                script.append(f"mkdir -p \"$(dirname '{f}')\" && touch '{f}'")
            for c, k in certs:
                script.append(f"mkdir -p \"$(dirname '{c}')\" \"$(dirname '{k}')\" && [ -f '{c}' ] || openssl req -x509 -newkey ec "
                              f"-pkeyopt ec_paramgen_curve:prime256v1 -nodes -keyout '{k}' -out '{c}' -days 1 -subj /CN=t >/dev/null 2>&1")
            script.append(f"if /veil -t -c /w/{name} >/w/{name}.log 2>&1; then echo 'ok   {md}:{line}'; "
                          f"else echo 'FAIL {md}:{line}'; sed 's/^/     /' /w/{name}.log | tail -5; fail=1; fi")
    script.append('exit "$fail"')
    open(os.path.join(work, "run.sh"), "w").write("\n".join(script) + "\n")
    r = subprocess.run(["docker", "run", "--rm", "-v", f"{work}:/w", "-v", f"{binary}:/veil:ro", "debian:13", "bash", "/w/run.sh"])
    print(f"{n} configuration example(s) checked")
    sys.exit(r.returncode)


if __name__ == "__main__":
    main()
