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
A rate-limited response (set GITHUB_TOKEN to raise the limit) is retried a few times
after the wait GitHub asks for, and still fails if the API stays unreadable.
"""
import glob
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request

# B-291: レート制限（403 + X-RateLimit-Remaining: 0、または 429）のときだけ待ってやり直す。
# 待ちが長すぎる（上限の回復が遠い）・回数を使い切ったら従来どおり失敗にする（照合せずに ok にしない）
RETRIES = 3
MAX_WAIT = 120

SPEC = re.compile(r"crates\.io の \[`([A-Za-z0-9_-]+)` ([0-9][0-9A-Za-z.+-]*)\]")
IGNORE = re.compile(r"^- advisory-ignore: ((?:GHSA|RUSTSEC)-[0-9A-Za-z-]+)（.+）", re.M)
REPO = re.compile(r"^- advisory-repo: ([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)（.+）", re.M)


def advisories(name, version):
    data = _get(
        "https://api.github.com/advisories?ecosystem=rust"
        f"&affects={name}@{version}&per_page=100"
    )
    if not isinstance(data, list):
        raise RuntimeError(f"unexpected advisory API response for {name}@{version}")
    return [a for a in data if not a.get("withdrawn_at")]


def rate_limit_wait(err, now=None):
    """Seconds to wait before retrying a rate-limited GitHub API response, or None when
    `err` is not a rate limit (a plain 403 such as a permission error is not retried)."""
    if err.code not in (403, 429):
        return None
    headers = err.headers or {}
    retry_after = headers.get("Retry-After")
    if retry_after and retry_after.strip().isdigit():
        return int(retry_after.strip())
    if headers.get("X-RateLimit-Remaining") == "0":
        reset = headers.get("X-RateLimit-Reset")
        if reset and reset.strip().isdigit():
            return max(0, int(reset.strip()) - int(now if now is not None else time.time())) + 1
        return 60
    if err.code == 429:
        return 60
    return None


def _get(url):
    req = urllib.request.Request(url, headers={"Accept": "application/vnd.github+json"})
    token = os.environ.get("GITHUB_TOKEN")
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    for attempt in range(RETRIES + 1):
        try:
            with urllib.request.urlopen(req, timeout=60) as resp:
                return json.load(resp)
        except urllib.error.HTTPError as e:
            wait = rate_limit_wait(e)
            if wait is None or wait > MAX_WAIT or attempt == RETRIES:
                raise
            print(f"warning: GitHub API rate limited ({e.code}); retrying in {wait}s", file=sys.stderr)
            time.sleep(wait)


def _ver(v):
    # "1.2" と "1.2.0" を同じ版として比べる（足りない桁は 0）
    t = tuple(int(x) for x in re.findall(r"\d+", v.split("-")[0])[:3])
    return (t + (0, 0, 0))[:3]


# 範囲の中の 1 つの制約（`>= 0.6.0`・`<0.5.16`・`0.15.0+`・裸の `0.20.0`）
_CONSTRAINT = re.compile(r"(>=|<=|>|<|=)?\s*v?(\d+(?:\.\d+){0,2}(?:-[0-9A-Za-z.]+)?)(\+)?")
_UPPER = ("<", "<=", "=")


def _satisfiable(constraints):
    """Whether some version meets every (op, version) constraint at once."""
    lo = hi = None  # (版, 端を含むか)
    for op, r in constraints:
        if op in (">=", ">", "="):
            c = (r, op != ">")
            if lo is None or c[0] > lo[0] or (c[0] == lo[0] and not c[1]):
                lo = c
        if op in ("<=", "<", "="):
            c = (r, op != "<")
            if hi is None or c[0] < hi[0] or (c[0] == hi[0] and not c[1]):
                hi = c
    return lo is None or hi is None or lo[0] < hi[0] or (lo[0] == hi[0] and lo[1] and hi[1])


def range_alternatives(spec):
    """Split a GitHub range into alternatives (any may match), each a list of (op, version)
    constraints that must all hold. Raises ValueError when no constraint can be read or the
    range contradicts itself."""
    alts = []
    for chunk in spec.split("||"):
        # "0.11.0 - 0.11.6" は両端を含む範囲
        chunk = re.sub(r"(\d[0-9A-Za-z.]*)\s+-\s+(\d[0-9A-Za-z.]*)", r">= \1, <= \2", chunk)
        cons = [(">=" if m.group(3) else (m.group(1) or "="), _ver(m.group(2)))
                for m in _CONSTRAINT.finditer(chunk.replace("&&", ","))]
        if not cons:
            continue
        # 並べた制約は全部を満たす（「<= 6.0.0, >= 1.0.0」）。ただし上流は「< 0.5.16, >= 0.6.0 < 0.6.3」と
        # 系列ごとの範囲を並べて書くことがあり、全部を満たす版は無い。そのときだけ、上限（か一致）の後に
        # 来た下限・一致を別の範囲の始まりとして読む（読んだままだと誰も該当せず、勧告を見逃す。B-400）
        if _satisfiable(cons):
            alts.append(cons)
            continue
        cur = []
        for op, r in cons:
            if cur and op in (">=", ">", "=") and any(o in _UPPER for o, _ in cur):
                alts.append(cur)
                cur = []
            cur.append((op, r))
        alts.append(cur)
    if not alts or not all(_satisfiable(a) for a in alts):
        raise ValueError(f"unreadable version range {spec!r}")
    return alts


def in_range(version, spec):
    """Whether `version` is in a GitHub range such as ">= 0.15.0", "< 0.24.4", "0.15.0+",
    "<0.19.1", ">= 0.1.0, < 0.29.3", ">= 0.26.0 && <= 0.29.0", "0.11.0 - 0.11.6", a bare
    "0.20.0", or ranges of several release series written one after another
    ("< 0.5.16, >= 0.6.0 < 0.6.3"). Raises ValueError for a range it cannot read."""
    v = _ver(version)
    cmp = {"<": lambda r: v < r, "<=": lambda r: v <= r, ">": lambda r: v > r,
           ">=": lambda r: v >= r, "=": lambda r: v == r}
    return any(all(cmp[op](r) for op, r in alt) for alt in range_alternatives(spec))


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
            spec = vuln.get("vulnerable_version_range") or ""
            try:
                hit = in_range(version, spec)
            except ValueError:
                # 読めない範囲を「該当しない」と見なさない（人が見て直した版を確かめる）
                a = dict(a, summary=f"{a.get('summary')} [unreadable range {spec!r}]")
                hit = True
            if hit:
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
