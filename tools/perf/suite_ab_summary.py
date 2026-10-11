#!/usr/bin/env python3
"""suite_ab.sh の結果を (構成, プロトコル) ごとに new / base の rps 中央値の比で集計する。

使い方: python3 tools/perf/suite_ab_summary.py <結果 TSV> <BASE> <NEW> [しきい値=0.95]

出力: 比の分布（プロトコル別の中央値・しきい値未満の数）と、しきい値未満・errors のある組の一覧。
"""
import csv
import statistics
import sys
from collections import defaultdict


def main():
    path, base, new = sys.argv[1], sys.argv[2], sys.argv[3]
    thr = float(sys.argv[4]) if len(sys.argv) > 4 else 0.95
    rps = defaultdict(list)
    errs = defaultdict(int)
    nginx = {}
    with open(path) as f:
        for row in csv.DictReader(f, delimiter="\t"):
            if row["target"] == "target":
                continue
            key = (row["config"], row["proto"])
            try:
                v = float(row["req_per_sec"])
            except ValueError:
                errs[(row["target"], key)] += 1
                continue
            try:
                if float(row["non2xx"] or 0) > 0:
                    errs[(row["target"], key)] += 1
            except ValueError:
                pass
            if row["target"] == "nginx":
                nginx.setdefault(key, []).append(v)
            rps[(row["target"], key)].append(v)

    pairs = []
    for (target, key), vals in rps.items():
        if target != f"veil_{base}":
            continue
        other = rps.get((f"veil_{new}", key))
        if not other:
            continue
        b, n = statistics.median(vals), statistics.median(other)
        if b > 0:
            pairs.append((key, b, n, n / b))
    pairs.sort()

    by_proto = defaultdict(list)
    for key, _, _, r in pairs:
        by_proto[key[1]].append(r)
    allr = [p[3] for p in pairs]
    print(f"# {new} / {base}（rps 中央値の比、{len(pairs)} 組）\n")
    print("| プロトコル | 組数 | 比の中央値 | 最小 | 最大 | <{:.2f} |".format(thr))
    print("|---|---|---|---|---|---|")
    for proto in sorted(by_proto):
        rs = by_proto[proto]
        print(f"| {proto} | {len(rs)} | {statistics.median(rs):.3f} | {min(rs):.3f} | "
              f"{max(rs):.3f} | {sum(r < thr for r in rs)} |")
    if allr:
        print(f"| **全体** | {len(allr)} | **{statistics.median(allr):.3f}** | {min(allr):.3f} | "
              f"{max(allr):.3f} | {sum(r < thr for r in allr)} |")

    low = [p for p in pairs if p[3] < thr]
    if low:
        print(f"\n## 比が {thr} 未満の組\n")
        print(f"| 構成 | プロトコル | {base} | {new} | 比 |")
        print("|---|---|---|---|---|")
        for (cfg, proto), b, n, r in low:
            print(f"| `{cfg}` | {proto} | {b:,.0f} | {n:,.0f} | {r:.3f} |")
    if errs:
        print("\n## errors / NA のある行\n")
        for (target, (cfg, proto)), c in sorted(errs.items()):
            print(f"- {target} `{cfg}` {proto}: {c} 行")


if __name__ == "__main__":
    main()
