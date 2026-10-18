#!/usr/bin/env python3
"""suite_ab の TSV から veil（NEW のターゲット）と nginx の同条件の相手との比を出す。
使い方: python3 tools/perf/vs_nginx.py <TSV> <veil のターゲット名（例: veil_glibc）>
同条件の相手: *_proxy* → base_proxy、*_compression（キャッシュ無し） → base_compression、h2c_* → 同名、
http3 の静的 → base（http3）、http3 の中継 → base_proxy（http3）、それ以外 → base（静的の TLS）
"""
import csv, statistics, sys
from collections import defaultdict
path, tgt = sys.argv[1], sys.argv[2]
v = defaultdict(list)
for r in csv.DictReader(open(path), delimiter="\t"):
    try: x = float(r["req_per_sec"])
    except ValueError: continue
    v[(r["target"], r["config"], r["proto"])].append(x)
med = {k: statistics.median(x) for k, x in v.items()}
def peer(cfg, proto):
    if cfg.startswith("h2c_"): return ("nginx", cfg, proto)
    if proto in ("grpc", "grpc_h3", "websocket"): return None
    if "_proxy" in cfg and cfg.endswith("_compression") and proto != "http3":
        return ("nginx", "base_proxy_compression", proto)
    if "_proxy" in cfg or cfg.endswith("feat_l4"):
        return ("nginx", "base_proxy", proto) if "_proxy" in cfg else ("nginx", "base", proto)
    if cfg.endswith("_compression"): return ("nginx", "base_compression", proto) if proto != "http3" else ("nginx", "base", proto)
    return ("nginx", "base", proto)
rows = []
for (t, cfg, proto), m in sorted(med.items()):
    if t != tgt: continue
    p = peer(cfg, proto)
    if not p or p not in med: continue
    rows.append((m / med[p], cfg, proto, m, med[p], p[1]))
rows.sort()
for r, cfg, proto, m, n, pc in rows:
    print(f"{r:5.2f}  {cfg:34s} {proto:8s} veil={m:9.1f} nginx({pc})={n:9.1f}")
by = defaultdict(list)
for r, cfg, proto, *_ in rows: by[proto].append(r)
print()
for proto, rs in sorted(by.items()):
    print(f"{proto:8s} n={len(rs):3d} median={statistics.median(rs):.2f} min={min(rs):.2f} below1={sum(1 for x in rs if x < 1)}")
