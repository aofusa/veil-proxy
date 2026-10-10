#!/usr/bin/env python3
"""alloc_ab.sh の TSV を集計する（F-183）。

各 metric をバリアントごとに中央値で並べ、先頭のバリアントを基準とした比（%）を添える。
rps は高いほど良く、*_kb は低いほど良い。
"""
import statistics
import sys
from collections import OrderedDict, defaultdict


def main(path):
    data = defaultdict(lambda: defaultdict(list))
    variants = OrderedDict()
    metrics = OrderedDict()
    with open(path) as f:
        next(f)
        for line in f:
            parts = line.rstrip("\n").split("\t")
            if len(parts) != 4:
                continue
            v, _rnd, m, val = parts
            try:
                x = float(val)
            except ValueError:
                continue
            variants[v] = None
            metrics[m] = None
            data[m][v].append(x)
    vs = list(variants)
    base = vs[0]
    print("| metric | " + " | ".join(vs) + " |")
    print("|---" * (len(vs) + 1) + "|")
    for m in metrics:
        row = []
        bmed = statistics.median(data[m][base]) if data[m][base] else 0
        for v in vs:
            xs = data[m][v]
            if not xs:
                row.append("-")
                continue
            med = statistics.median(xs)
            cell = f"{med:,.0f}"
            if len(xs) > 1:
                cell += f" (±{statistics.pstdev(xs):,.0f})"
            if v != base and bmed:
                cell += f" [{(med / bmed - 1) * 100:+.1f}%]"
            row.append(cell)
        print(f"| {m} | " + " | ".join(row) + " |")


if __name__ == "__main__":
    main(sys.argv[1])
