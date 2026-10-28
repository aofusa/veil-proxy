#!/usr/bin/env python3
# F-277: vendoring 版の勧告の照合
"""Match vendored crates (third_party/*) against GitHub security advisories.

The vendored crates are renamed or path dependencies, so cargo-audit does not
match them, and some upstream advisories (quiche) are published only as GHSA. The
upstream name and version come from the "crates.io の [`name` version]" line of each
README.veil.md; "- advisory-ignore: <ID>（reason）" lines in the same file skip an
advisory that the vendored copy has fixed. "- advisory-repo: <owner>/<repo>（reason）" also
checks the advisories published on the upstream repository, which GitHub does not always
copy into the global database (quiche's GHSA-4fgf-9xrr-88gf and GHSA-4q5x-gp38-rfp4). Exits non-zero when an unignored advisory
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
REPO = re.compile(r"^- advisory-repo: ([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)（.+）", re.M)


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


def _get(url):
    req = urllib.request.Request(url, headers={"Accept": "application/vnd.github+json"})
    token = os.environ.get("GITHUB_TOKEN")
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    with urllib.request.urlopen(req, timeout=60) as resp:
        return json.load(resp)


def _ver(v):
    return tuple(int(x) for x in re.findall(r"\d+", v.split("-")[0])[:3])


def in_range(version, spec):
    """Whether `version` is in a GitHub range such as ">= 0.15.0", "< 0.24.4", "0.15.0+",
    "<0.19.1", ">= 0.1.0, < 0.29.3", ">= 0.26.0 && <= 0.29.0" or a bare "0.20.0"."""
    v = _ver(version)
    for part in re.split(r",|&&", spec):
        part = part.strip()
        if not part:
            continue
        if part.endswith("+"):
            op, ref = ">=", part[:-1]
        else:
            m = re.match(r"(>=|<=|>|<|=)?\s*(.+)", part)
            op, ref = m.group(1) or "=", m.group(2)
        r = _ver(ref)
        if not {"<": v < r, "<=": v <= r, ">": v > r, ">=": v >= r, "=": v == r}[op]:
            return False
    return True


def patched_in_series(version, patched):
    """Whether `version` is at or after a patched version of its own release series
    (same major, or same major.minor for 0.x)."""
    v = _ver(version)
    series = v[:2] if v and v[0] == 0 else v[:1]
    for p in patched.split(","):
        p = p.strip().lstrip(">= ").strip()
        if not re.match(r"\d", p):
            continue
        pv = _ver(p)
        if (pv[:2] if pv and pv[0] == 0 else pv[:1]) == series and v >= pv:
            return True
    return False


def repo_advisories(repo, name, version):
    data = _get(
        f"https://api.github.com/repos/{repo}/security-advisories?per_page=100&state=published"
    )
    if not isinstance(data, list):
        raise RuntimeError(f"unexpected repository advisory API response for {repo}")
    found = []
    for a in data:
        if a.get("withdrawn_at"):
            continue
        for vuln in a.get("vulnerabilities") or []:
            pkg = (vuln.get("package") or {}).get("name") or ""
            # 上流の勧告はパッケージ名を `cloudflare-quiche` のように書くことがある
            if pkg != name and not pkg.endswith("-" + name):
                continue
            # 範囲に上限が無く（`0.15.0+`）、直した版だけが書いてある勧告がある。直した版は系列ごとに並ぶ
            # （`38.0.4, 37.0.3, 36.0.3`）ので、同じ系列で直した版以上なら該当しない
            if patched_in_series(version, vuln.get("patched_versions") or ""):
                continue
            if in_range(version, vuln.get("vulnerable_version_range") or ""):
                found.append(a)
                break
    return found


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
            seen = {a["ghsa_id"] for a in found}
            for repo in REPO.findall(text):
                found += [a for a in repo_advisories(repo, name, version) if a["ghsa_id"] not in seen]
                seen = {a["ghsa_id"] for a in found}
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
