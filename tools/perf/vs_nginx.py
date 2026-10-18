#!/usr/bin/env python3
"""suite_ab の TSV から veil（NEW のターゲット）と nginx の同条件の相手との比を出す。
使い方: python3 tools/perf/vs_nginx.py <TSV> <veil のターゲット名（例: veil_glibc）>
同条件の相手: 中継（configs/<cfg>.toml のルートが Proxy）→ base_proxy、*_compression（キャッシュ無し） → base_compression、h2c_* → 同名、
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
import os
CONFIG_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "configs")
def is_proxy(cfg):
    """構成のルートの動作が Proxy か（名前でなく configs/<cfg>.toml を見る。feat_buffering 等も中継）"""
    try:
        txt = open(os.path.join(CONFIG_DIR, cfg + ".toml")).read()
    except OSError:
        return "_proxy" in cfg
    return 'type = "Proxy"' in txt
def peer(cfg, proto):
    if cfg.startswith("h2c_"): return ("nginx", cfg, proto)
    if proto in ("grpc", "grpc_h3", "websocket"): return None
    proxy = is_proxy(cfg)
    if proxy and cfg.endswith("_compression") and proto != "http3":
        return ("nginx", "base_proxy_compression", proto)
    if proxy:
        return ("nginx", "base_proxy", proto)
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
