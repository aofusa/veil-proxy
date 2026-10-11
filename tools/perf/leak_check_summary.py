#!/usr/bin/env python3
"""leak_check_summary.py — leak_check.sh の TSV を集計する（F-185）。

シナリオごとに、先頭 WARMUP サイクルを除いた採取値をサイクル番号に回帰し、
1 サイクルあたりの増分（傾き）と、最初・最後の値を出す。リークはサイクルごとに一定量ずつ
増えるので傾きに出る（プール・キャッシュが満ちる飽和型の増加はウォームアップで落ちる）。

使い方: leak_check_summary.py <TSV> [WARMUP=5]
"""
import csv
import sys

COLS = [
    ("live_bytes", 1024, "KB"),
    ("live_allocs", 1, ""),
    ("heap_active", 1024, "KB"),
    ("anon_kb", 1, "KB"),
    ("rss_kb", 1, "KB"),
    ("segments", 1, ""),
    ("heaps", 1, ""),
    ("fds", 1, ""),
    ("threads", 1, ""),
    ("conns", 1, ""),
]


def slope(xs, ys):
    n = len(xs)
    if n < 2:
        return 0.0
    mx, my = sum(xs) / n, sum(ys) / n
    den = sum((x - mx) ** 2 for x in xs)
    return sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / den if den else 0.0


def main():
    path = sys.argv[1]
    warm = int(sys.argv[2]) if len(sys.argv) > 2 else 5
    rows = list(csv.DictReader(open(path), delimiter="\t"))
    order = []
    by = {}
    for r in rows:
        s = r["scenario"]
        if s not in by:
            by[s] = []
            order.append(s)
        by[s].append(r)
    print(f"# 1 サイクルあたりの傾き（先頭 {warm} サイクル除外）と 最初→最後（除外後）")
    head = "| シナリオ | n | " + " | ".join(c for c, _, _ in COLS) + " |"
    print(head)
    print("|" + "---|" * (len(COLS) + 2))
    for s in order:
        rs = [r for r in by[s] if int(r["cycle"]) > warm] or by[s]
        xs = [int(r["cycle"]) for r in rs]
        cells = []
        for c, div, unit in COLS:
            ys = [int(r[c] or 0) / div for r in rs]
            k = slope(xs, ys)
            cells.append(f"{k:+.1f}{unit}/c ({ys[0]:.0f}→{ys[-1]:.0f})")
        print(f"| {s} | {len(rs)} | " + " | ".join(cells) + " |")


if __name__ == "__main__":
    main()
