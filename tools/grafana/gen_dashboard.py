#!/usr/bin/env python3
"""F-256: examples/grafana/veil-overview.json（Grafana のダッシュボードの例）を作る。

ダッシュボードの JSON は手で直さず、このファイルを直して作り直す（`python3 tools/grafana/gen_dashboard.py`）。
Helm chart の写し（deploy/helm/veil/dashboards/）も同じものにする。メトリクスの名前・ラベルは
docs/guide/observability.md の表と同じ。試験（tools/grafana/test-dashboard.sh）は全パネルの式を Prometheus に問い合わせる。
"""
import json
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
DS = {"type": "prometheus", "uid": "${datasource}"}
SEL = 'job=~"$job",instance=~"$instance"'
HSEL = SEL + ',host=~"$host"'
USEL = SEL + ',upstream=~"$upstream"'
RI = "$__rate_interval"

panels = []
_id = [0]
_y = [0]


def nid():
    _id[0] += 1
    return _id[0]


def row(title, collapsed, children=()):
    p = {"type": "row", "title": title, "id": nid(), "collapsed": collapsed,
         "gridPos": {"h": 1, "w": 24, "x": 0, "y": _y[0]}, "panels": []}
    _y[0] += 1
    if collapsed:
        p["panels"] = list(children)
        panels.append(p)
    else:
        panels.append(p)
        panels.extend(children)
    return p


def ts(title, targets, unit="short", w=12, h=8, x=0, desc="", stack=False, kind="timeseries"):
    p = {
        "type": kind, "title": title, "id": nid(), "datasource": DS, "description": desc,
        "gridPos": {"h": h, "w": w, "x": x, "y": _y[0]},
        "fieldConfig": {"defaults": {"unit": unit}, "overrides": []},
        "options": {"legend": {"displayMode": "table", "placement": "bottom", "calcs": ["mean", "max", "lastNotNull"]},
                    "tooltip": {"mode": "multi", "sort": "desc"}},
        "targets": [{"datasource": DS, "expr": e, "legendFormat": l, "refId": chr(65 + i)}
                    for i, (e, l) in enumerate(targets)],
    }
    if kind == "timeseries" and stack:
        p["fieldConfig"]["defaults"]["custom"] = {"stacking": {"mode": "normal"}, "fillOpacity": 20}
    if kind == "stat":
        p["options"] = {"reduceOptions": {"calcs": ["lastNotNull"], "values": False}, "colorMode": "value",
                        "graphMode": "area", "textMode": "auto"}
    return p


def line(*ps):
    """横に並べたパネルの行。x を割り振って y を進める"""
    x = 0
    h = 0
    for p in ps:
        p["gridPos"]["x"] = x
        p["gridPos"]["y"] = _y[0]
        x += p["gridPos"]["w"]
        h = max(h, p["gridPos"]["h"])
    _y[0] += h
    return list(ps)


def q(quantile, metric, by, sel):
    return f'histogram_quantile({quantile}, sum by (le{", " + by if by else ""}) (rate({metric}_bucket{{{sel}}}[{RI}])))'


# ---- Traffic ----
traffic = []
traffic += line(
    ts("Requests per second by status", [
        (f'sum by (status) (rate(veil_proxy_http_requests_total{{{HSEL}}}[{RI}]))', "{{status}}")], "reqps", w=12, stack=True,
       desc="veil_proxy_http_requests_total by status code."),
    ts("Requests per second by host", [
        (f'sum by (host) (rate(veil_proxy_http_requests_total{{{HSEL}}}[{RI}]))', "{{host}}")], "reqps", w=12,
       desc="host is the route's host condition, or \"other\" (label values are bounded)."),
)
traffic += line(
    ts("5xx ratio", [
        (f'sum(rate(veil_proxy_http_requests_total{{{HSEL},status=~"5.."}}[{RI}])) / '
         f'clamp_min(sum(rate(veil_proxy_http_requests_total{{{HSEL}}}[{RI}])), 1e-9)', "5xx")], "percentunit", w=6, h=6, kind="stat"),
    ts("Requests per second", [
        (f'sum(rate(veil_proxy_http_requests_total{{{HSEL}}}[{RI}]))', "rps")], "reqps", w=6, h=6, kind="stat"),
    ts("Response size (p50 / p95)", [
        (q(0.5, "veil_proxy_http_response_size_bytes", "", SEL), "p50"),
        (q(0.95, "veil_proxy_http_response_size_bytes", "", SEL), "p95")], "bytes", w=6, h=6),
    ts("Request size (p50 / p95)", [
        (q(0.5, "veil_proxy_http_request_size_bytes", "", SEL), "p50"),
        (q(0.95, "veil_proxy_http_request_size_bytes", "", SEL), "p95")], "bytes", w=6, h=6),
)
row("Traffic", False, traffic)

# ---- Latency ----
lat = []
lat += line(
    ts("Request duration p50 / p95 / p99", [
        (q(0.5, "veil_proxy_http_request_duration_seconds", "", HSEL), "p50"),
        (q(0.95, "veil_proxy_http_request_duration_seconds", "", HSEL), "p95"),
        (q(0.99, "veil_proxy_http_request_duration_seconds", "", HSEL), "p99")], "s", w=12),
    ts("Request duration p99 by host", [
        (q(0.99, "veil_proxy_http_request_duration_seconds", "host", HSEL), "{{host}}")], "s", w=12),
)
lat += line(
    ts("Active connections by host", [
        (f'sum by (host) (veil_proxy_http_active_connections{{{HSEL}}})', "{{host}}")], "short", w=24),
)
row("Latency and connections", False, lat)

# ---- Upstream ----
up = []
up += line(
    ts("Upstream server health (1 = healthy)", [
        (f'min by (upstream, server) (veil_proxy_http_upstream_health{{{USEL}}})', "{{upstream}} {{server}}")], "short", w=12),
    ts("Circuit breaker state (0 closed, 1 open, 2 half-open)", [
        (f'max by (upstream) (veil_circuit_breaker_state{{{USEL}}})', "{{upstream}}")], "short", w=12),
)
up += line(
    ts("Retries per second", [
        (f'sum by (upstream, result) (rate(veil_retry_total{{{USEL}}}[{RI}]))', "{{upstream}} {{result}}")], "ops", w=8),
    ts("Circuit breaker opened per second", [
        (f'sum by (upstream) (rate(veil_circuit_breaker_open_total{{{USEL}}}[{RI}]))', "{{upstream}}")], "ops", w=8),
    ts("Ejected servers (outlier detection)", [
        (f'sum by (upstream) (veil_outlier_ejected{{{USEL}}})', "{{upstream}}")], "short", w=8),
)
up += line(
    ts("Connection pool hit ratio", [
        (f'sum by (upstream) (rate(veil_connection_pool_hits_total{{{USEL}}}[{RI}])) / clamp_min(sum by (upstream) '
         f'(rate(veil_connection_pool_hits_total{{{USEL}}}[{RI}]) + rate(veil_connection_pool_misses_total{{{USEL}}}[{RI}])), 1e-9)',
         "{{upstream}}")], "percentunit", w=12),
    ts("Connection pool size", [
        (f'sum by (upstream) (veil_connection_pool_size{{{USEL}}})', "{{upstream}}")], "short", w=12),
)
row("Upstreams", False, up)

# ---- Cache ----
cache = []
cache += line(
    ts("Cache hit ratio", [
        (f'sum(rate(veil_proxy_cache_hits_total{{{HSEL}}}[{RI}])) / clamp_min(sum(rate(veil_proxy_cache_hits_total{{{HSEL}}}[{RI}])) + '
         f'sum(rate(veil_proxy_cache_misses_total{{{HSEL}}}[{RI}])), 1e-9)', "hit ratio")], "percentunit", w=8),
    ts("Cache stores and evictions per second", [
        (f'sum by (storage) (rate(veil_proxy_cache_stores_total{{{HSEL}}}[{RI}]))', "store {{storage}}"),
        (f'sum by (reason) (rate(veil_proxy_cache_evictions_total{{{SEL}}}[{RI}]))', "evict {{reason}}")], "ops", w=8),
    ts("Cache size", [
        (f'sum by (storage) (veil_proxy_cache_size_bytes{{{SEL}}})', "{{storage}} bytes")], "bytes", w=8),
)
cache += line(
    ts("Cache entries", [(f'sum by (storage) (veil_proxy_cache_entries{{{SEL}}})', "{{storage}}")], "short", w=12),
    ts("Buffered requests per second", [
        (f'sum by (host) (rate(veil_proxy_buffering_used_total{{{HSEL}}}[{RI}]))', "{{host}}")], "reqps", w=12),
)
row("Cache and buffering", True, cache)

# ---- gRPC ----
grpc = line(
    ts("gRPC requests per second by status code", [
        (f'sum by (status_code) (rate(veil_grpc_requests_total{{{SEL}}}[{RI}]))', "{{status_code}}")], "reqps", w=12, stack=True),
    ts("gRPC stream duration p95", [
        (q(0.95, "veil_grpc_stream_duration_seconds", "method", SEL), "{{method}}")], "s", w=12),
)
row("gRPC", True, grpc)

# ---- WASM ----
wasm = line(
    ts("Proxy-Wasm filter time p99", [
        (q(0.99, "veil_wasm_filter_duration_seconds", "filter, phase", SEL), "{{filter}} {{phase}}")], "s", w=12),
    ts("Proxy-Wasm fuel per second", [
        (f'sum by (filter, phase) (rate(veil_wasm_fuel_consumed_total{{{SEL}}}[{RI}]))', "{{filter}} {{phase}}")], "short", w=12),
)
row("Proxy-Wasm", True, wasm)

# ---- VDS ----
vds = line(
    ts("VDS connected", [(f'max(veil_proxy_vds_connected{{{SEL}}})', "connected")], "short", w=6, h=6, kind="stat"),
    ts("Seconds since the last VDS update", [
        (f'time() - max(veil_proxy_vds_last_update_timestamp_seconds{{{SEL}}})', "age")], "s", w=6, h=6, kind="stat"),
    ts("VDS updates per second", [
        (f'sum by (result) (rate(veil_proxy_vds_updates_total{{{SEL}}}[{RI}]))', "{{result}}")], "ops", w=12, h=6),
)
row("VDS (dynamic configuration)", True, vds)

def var_query(name, label, query, multi=True):
    return {
        "name": name, "label": label, "type": "query", "datasource": DS,
        "query": {"query": query, "refId": name}, "definition": query,
        "refresh": 2, "includeAll": True, "multi": multi, "allValue": ".*",
        "current": {"selected": True, "text": ["All"], "value": ["$__all"]}, "sort": 1,
    }


dashboard = {
    "title": "veil overview",
    "uid": "veil-overview",
    "description": "Traffic, latency, upstreams, cache, gRPC, Proxy-Wasm and VDS of veil (https://github.com/aofusa/veil-proxy).",
    "tags": ["veil", "proxy"],
    "editable": True,
    "graphTooltip": 1,
    "schemaVersion": 39,
    "version": 1,
    "time": {"from": "now-1h", "to": "now"},
    "refresh": "30s",
    "links": [{"title": "veil metrics", "type": "link", "targetBlank": True,
               "url": "https://github.com/aofusa/veil-proxy/blob/main/docs/guide/observability.md"}],
    "templating": {"list": [
        {"name": "datasource", "label": "Data source", "type": "datasource", "query": "prometheus",
         "current": {}, "hide": 0, "refresh": 1},
        var_query("job", "Job", "label_values(veil_proxy_http_requests_total, job)"),
        var_query("instance", "Instance", 'label_values(veil_proxy_http_requests_total{job=~"$job"}, instance)'),
        var_query("host", "Host", 'label_values(veil_proxy_http_requests_total{job=~"$job"}, host)'),
        var_query("upstream", "Upstream", 'label_values(veil_proxy_http_upstream_health{job=~"$job"}, upstream)'),
    ]},
    "panels": panels,
}

out = json.dumps(dashboard, indent=2, ensure_ascii=True) + "\n"
paths = [os.path.join(ROOT, "examples/grafana/veil-overview.json"), os.path.join(ROOT, "deploy/helm/veil/dashboards/veil-overview.json")]
if "--check" in sys.argv:
    bad = [p for p in paths if not os.path.exists(p) or open(p).read() != out]
    if bad:
        print("out of date (run python3 tools/grafana/gen_dashboard.py):", *bad)
        sys.exit(1)
    print("ok   dashboards are up to date")
else:
    for p in paths:
        os.makedirs(os.path.dirname(p), exist_ok=True)
        open(p, "w").write(out)
    print("wrote", *paths)
