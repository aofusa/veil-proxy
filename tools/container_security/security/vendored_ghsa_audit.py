#!/usr/bin/env python3
# F-277: vendoring 版の勧告の照合
"""Match vendored crates (third_party/*) against GitHub security advisories.

The vendored crates are renamed or path dependencies, so cargo-audit does not
match them, and some upstream advisories (quiche) are published only as GHSA. The
upstream name and version come from the "crates.io の [`name` version]" line of each
README.veil.md; "- advisory-ignore: <ID>（reason）" lines in the same file skip an
advisory that the vendored copy has fixed. Exits non-zero when an unignored advisory
matches or the advisory API cannot be read (never reports success without checking).
"""
import glob
import json
import os
import re
import sys
import urllib.request

SPEC = re.compile(r"crates\.io の \[`([A-Za-z0-9_-]+)` ([0-9][0-9A-Za-z.+-]*)\]")
IGNORE = re.compile(r"^- advisory-ignore: ((?:GHSA|RUSTSEC)-[0-9A-Za-z-]+)（.+）", re.M)


def advisories(name, version):
    url = (
        "https://api.github.com/advisories?ecosystem=rust"
        f"&affects={name}@{version}&per_page=100"
    )
    req = urllib.request.Request(url, headers={"Accept": "application/vnd.github+json"})
    token = os.environ.get("GITHUB_TOKEN")
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    with urllib.request.urlopen(req, timeout=60) as resp:
        data = json.load(resp)
    if not isinstance(data, list):
        raise RuntimeError(f"unexpected advisory API response for {name}@{version}")
    return [a for a in data if not a.get("withdrawn_at")]


def main(root):
    failed = False
    checked = 0
    for readme in sorted(glob.glob(os.path.join(root, "third_party", "*", "README.veil.md"))):
        text = open(readme, encoding="utf-8").read()
        spec = SPEC.search(text)
        if not spec:
            continue
        name, version = spec.groups()
        ignored = set(IGNORE.findall(text))
        try:
            found = advisories(name, version)
        except Exception as e:  # ネットワーク・API の失敗は「照合できなかった」なので失敗にする
            print(f"error: cannot read advisories for {name}@{version}: {e}")
            return 1
        checked += 1
        ids = {a["ghsa_id"] for a in found}
        for a in found:
            mark = "ignored" if a["ghsa_id"] in ignored or a.get("cve_id") in ignored else "MATCH"
            print(f"{mark}: {name}@{version} {a['ghsa_id']} ({a.get('severity')}) {a.get('summary')}")
            if mark == "MATCH":
                failed = True
        for stale in sorted(i for i in ignored if i.startswith("GHSA-") and i not in ids):
            # 外す理由が無くなった（版を上げた等）行は消す
            print(f"warning: {readme}: advisory-ignore {stale} no longer matches {name}@{version}")
        if not found:
            print(f"ok: {name}@{version} has no GitHub advisories")
    if checked == 0:
        print("error: no vendored crate found in third_party/*/README.veil.md")
        return 1
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1] if len(sys.argv) > 1 else "."))
