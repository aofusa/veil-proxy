#!/usr/bin/env python3
"""VDS（Veil Discovery Service）の参照制御プレーン（F-04）。標準ライブラリのみ（Python 3.11+）。

スナップショット（veil の config.toml と同じ書式の `[[route]]` と `[upstreams.<name>]` だけを書いた
TOML ファイル）を監視し、変更があれば長輪詢中の veil へ配信する。xDS の REST-JSON /
State-of-the-World と同じ意味論（version_info・nonce・ACK/NACK）。

    python3 tools/vds/vds_server.py --snapshot snapshot.toml --listen 0.0.0.0:18000 [--token-file token]

エンドポイント:
  POST /v1/discovery   DiscoveryRequest → 200 DiscoveryResponse / 304（変化なし・保留タイムアウト）
  GET  /v1/status      ノードごとの ACK 済みバージョン・最後の NACK（JSON）
  GET  /healthz        200
"""
import argparse
import hashlib
import hmac
import json
import os
import sys
import threading
import time
import tomllib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

TYPE_SNAPSHOT = "type.veil.dev/veil.v1.Snapshot"
TYPE_ROUTE = "type.veil.dev/veil.v1.Route"
TYPE_UPSTREAM = "type.veil.dev/veil.v1.Upstream"


class Store:
    """スナップショットの現在版（ファイルの mtime を監視して読み直す）。"""

    def __init__(self, path):
        self.path = path
        self.cond = threading.Condition()
        self.version = ""
        self.resources = []
        self.mtime = None
        self.nodes = {}
        self.nonce_seq = 0
        self.load_error = None
        self.reload()

    def reload(self):
        try:
            st = os.stat(self.path)
        except FileNotFoundError:
            return
        key = (st.st_mtime_ns, st.st_size, st.st_ino)
        if key == self.mtime:
            return
        with open(self.path, "rb") as f:
            raw = f.read()
        try:
            doc = tomllib.loads(raw.decode())
        except Exception as e:  # 構文エラーは配信しない（前の版を保つ）
            self.load_error = str(e)
            self.mtime = key
            print(f"[vds] snapshot parse error: {e}", file=sys.stderr, flush=True)
            return
        resources = []
        for r in doc.get("route", []):
            resources.append({"@type": TYPE_ROUTE, **r})
        for name, u in doc.get("upstreams", {}).items():
            resources.append({"@type": TYPE_UPSTREAM, "name": name, **u})
        version = hashlib.sha256(raw).hexdigest()[:16]
        with self.cond:
            self.mtime = key
            self.load_error = None
            if version != self.version:
                self.version = version
                self.resources = resources
                print(f"[vds] snapshot version {version} ({len(resources)} resources)", flush=True)
                self.cond.notify_all()

    def wait_change(self, known_version, timeout):
        deadline = time.monotonic() + timeout
        with self.cond:
            while self.version == known_version:
                left = deadline - time.monotonic()
                if left <= 0:
                    return False
                self.cond.wait(min(left, 1.0))
            return True


def make_handler(store, token, hold_max):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, fmt, *args):
            pass

        def _send(self, code, obj=None):
            body = b"" if obj is None else json.dumps(obj).encode()
            self.send_response(code)
            if obj is not None:
                self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            if self.path == "/healthz":
                return self._send(200, {"ok": True})
            if self.path == "/v1/status":
                with store.cond:
                    return self._send(200, {"version": store.version, "nodes": store.nodes,
                                            "load_error": store.load_error})
            self._send(404, {"error": "not found"})

        def do_POST(self):
            if self.path.rstrip("/") != "/v1/discovery":
                return self._send(404, {"error": "not found"})
            if token is not None:
                got = self.headers.get("Authorization", "")
                if not hmac.compare_digest(got, f"Bearer {token}"):
                    return self._send(401, {"error": "unauthorized"})
            n = int(self.headers.get("Content-Length", "0"))
            if n > 1 << 20:
                return self._send(413, {"error": "too large"})
            try:
                req = json.loads(self.rfile.read(n) or b"{}")
            except ValueError:
                return self._send(400, {"error": "invalid json"})
            node = (req.get("node") or {}).get("id", "unknown")
            version = req.get("version_info", "")
            err = req.get("error_detail")
            with store.cond:
                st = store.nodes.setdefault(node, {"acked_version": "", "last_nack": None, "nacks": 0})
                if err:
                    st["last_nack"] = {"nonce": req.get("response_nonce", ""), "message": err.get("message", "")}
                    st["nacks"] += 1
                    st["rejected_version"] = store.version
                    print(f"[vds] NACK from {node}: {err.get('message')}", flush=True)
                else:
                    st["acked_version"] = version
                # NACK を受けた版は、内容が変わるまで送り直さない（xDS と同じ）。
                rejected = st.get("rejected_version") == store.version
            known = store.version if rejected else version
            hold = min(float(req.get("poll_timeout_secs", hold_max)), hold_max)
            if known == store.version and not store.wait_change(known, hold):
                return self._send(304)
            with store.cond:
                store.nonce_seq += 1
                resp = {
                    "version_info": store.version,
                    "nonce": str(store.nonce_seq),
                    "type_url": TYPE_SNAPSHOT,
                    "resources": store.resources,
                }
            self._send(200, resp)

    return Handler


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--snapshot", required=True)
    ap.add_argument("--listen", default="127.0.0.1:18000")
    ap.add_argument("--token-file")
    ap.add_argument("--hold", type=float, default=30.0, help="長輪詢の保留時間の上限（秒）")
    ap.add_argument("--watch-interval", type=float, default=0.5)
    args = ap.parse_args()
    token = None
    if args.token_file:
        with open(args.token_file) as f:
            token = f.read().strip()
    store = Store(args.snapshot)

    def watch():
        while True:
            time.sleep(args.watch_interval)
            try:
                store.reload()
            except Exception as e:  # noqa: BLE001 監視は止めない
                print(f"[vds] reload error: {e}", file=sys.stderr, flush=True)

    threading.Thread(target=watch, daemon=True).start()
    host, port = args.listen.rsplit(":", 1)
    srv = ThreadingHTTPServer((host.strip("[]"), int(port)), make_handler(store, token, args.hold))
    srv.daemon_threads = True
    print(f"[vds] listening on {args.listen}", flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
