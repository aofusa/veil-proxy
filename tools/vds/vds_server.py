#!/usr/bin/env python3
"""VDS（Veil Discovery Service）の参照制御プレーン（F-04）。標準ライブラリのみ（Python 3.11+）。

スナップショット（veil の config.toml と同じ書式の `[[route]]` と `[upstreams.<name>]` だけを書いた
TOML ファイル）を監視し、変更があれば長輪詢中の veil へ配信する。xDS の REST-JSON /
State-of-the-World と同じ意味論（version_info・nonce・ACK/NACK）。

    python3 tools/vds/vds_server.py --snapshot snapshot.toml --listen 0.0.0.0:18000 --token-file token

認証（B-470）: --token-file は全ノードで共有するトークン（持つ者はどのノード ID も名乗れる）。--node-tokens は
ノードごとのトークン（1 行に `<ノード ID> <トークン>`）で、要求のノード ID はトークンのノードに限る。どちらも
無ければループバック以外では待ち受けない（--allow-unauthenticated で明示したときだけ）。

エンドポイント:
  POST /v1/discovery   DiscoveryRequest → 200 DiscoveryResponse / 304（変化なし・保留タイムアウト）
  GET  /v1/status      ノードごとの ACK 済みバージョン・最後の NACK（JSON。--token-file のときはトークンが要る）
  GET  /healthz        200
"""
import argparse
import hashlib
import hmac
import ipaddress
import json
import math
import os
import sys
import threading
import time
import tomllib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

TYPE_SNAPSHOT = "type.veil.dev/veil.v1.Snapshot"
TYPE_ROUTE = "type.veil.dev/veil.v1.Route"
TYPE_UPSTREAM = "type.veil.dev/veil.v1.Upstream"


def _reject_constant(name):
    raise ValueError(f"{name} is not allowed")


def load_node_tokens(path):
    """Read a --node-tokens file: one '<node-id> <token>' per line ('#' starts a comment)."""
    out = []
    with open(path) as f:
        for i, line in enumerate(f, 1):
            line = line.split("#", 1)[0].strip()
            if not line:
                continue
            parts = line.split()
            if len(parts) != 2:
                raise ValueError(f"{path}:{i}: expected '<node-id> <token>'")
            node, tok = parts
            if any(t == tok or n == node for t, n in out):
                raise ValueError(f"{path}:{i}: duplicate node id or token")
            out.append((tok, node))
    if not out:
        raise ValueError(f"{path}: no node tokens")
    return out


def record(st, version, nack, current):
    """Record a request in a node's state and return whether ``current`` must not be sent to it (it was NACKed)."""
    if nack is None:
        # B-470: 前に ACK したノードが空の版で来たら、新しく起動したプロセス（前のプロセスの NACK を持ち越さない）。
        # NACK した版を ACK したら、その NACK は消す
        if (version == "" and st["acked_version"] != "") or st.get("rejected_version") == version:
            st["rejected_version"] = None
        st["acked_version"] = version
    else:
        st["last_nack"] = nack
        st["nacks"] += 1
        st["rejected_version"] = current
    # NACK を受けた版は、内容が変わるまで送り直さない（xDS と同じ）。
    return st.get("rejected_version") == current


def _is_loopback(host):
    if host == "localhost":
        return True
    try:
        return ipaddress.ip_address(host).is_loopback
    except ValueError:
        return False


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


def make_handler(store, token, hold_max, node_tokens=()):
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

        def _caller(self):
            """(authenticated, the only node id the caller may act as or None for any)."""
            got = self.headers.get("Authorization", "")
            if node_tokens:
                # 全部と比べる（一致した位置で時間を変えない）
                found = None
                for tok, node in node_tokens:
                    if hmac.compare_digest(got.encode(), f"Bearer {tok}".encode()):
                        found = node
                return (found is not None, found)
            if token is None:
                return (True, None)
            return (hmac.compare_digest(got.encode(), f"Bearer {token}".encode()), None)

        def _authorized(self):
            return self._caller()[0]

        def do_GET(self):
            if self.path == "/healthz":
                return self._send(200, {"ok": True})
            if self.path == "/v1/status":
                # B-456: ノードの一覧・NACK の文・解析エラーを返すので discovery と同じトークンを求める
                if not self._authorized():
                    return self._send(401, {"error": "unauthorized"})
                with store.cond:
                    return self._send(200, {"version": store.version, "nodes": store.nodes,
                                            "load_error": store.load_error})
            self._send(404, {"error": "not found"})

        def do_POST(self):
            if self.path.rstrip("/") != "/v1/discovery":
                return self._send(404, {"error": "not found"})
            ok, only_node = self._caller()
            if not ok:
                return self._send(401, {"error": "unauthorized"})
            try:
                n = int(self.headers.get("Content-Length", "0"))
            except ValueError:
                return self._send(400, {"error": "invalid content-length"})
            if n < 0 or n > 1 << 20:
                return self._send(413, {"error": "too large"})
            try:
                # B-456: NaN・Infinity の字句は JSON ではない（保留の時間に NaN を入れると wait_change が待たずに回り続けた）
                req = json.loads(self.rfile.read(n) or b"{}", parse_constant=_reject_constant)
            except ValueError:
                return self._send(400, {"error": "invalid json"})
            node_obj = req.get("node") if isinstance(req, dict) else None
            err = req.get("error_detail") if isinstance(req, dict) else None
            node = node_obj.get("id", "unknown") if isinstance(node_obj, dict) else "unknown"
            version = req.get("version_info", "") if isinstance(req, dict) else ""
            message = err.get("message", "") if isinstance(err, dict) else ""
            if not isinstance(req, dict) or not all(isinstance(v, str) for v in (node, version, message)):
                return self._send(400, {"error": "invalid request"})
            if err is not None and not isinstance(err, dict):
                return self._send(400, {"error": "invalid error_detail"})
            # B-470: ノードごとのトークンは、そのノードとしてだけ要求・ACK・NACK できる
            if only_node is not None and node != only_node:
                return self._send(403, {"error": "the node id does not match the token"})
            try:
                t = float(req.get("poll_timeout_secs", hold_max))
            except (TypeError, ValueError):
                return self._send(400, {"error": "invalid poll_timeout_secs"})
            hold = min(max(t, 0.0), hold_max) if math.isfinite(t) else hold_max
            with store.cond:
                st = store.nodes.setdefault(node, {"acked_version": "", "last_nack": None, "nacks": 0})
                nack = {"nonce": req.get("response_nonce", ""), "message": message} if err else None
                rejected = record(st, version, nack, store.version)
                if err:
                    # B-456: ノード ID・NACK の文は要求の値。repr で制御文字を書き換え、偽の行を足させない
                    print(f"[vds] NACK from {node[:512]!r}: {message[:512]!r}", flush=True)
            known = store.version if rejected else version
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
    ap.add_argument("--token-file", help="one bearer token shared by all nodes (a holder can act as any node id)")
    ap.add_argument("--node-tokens", help="one '<node-id> <token>' per line; a token can only act as its node id")
    ap.add_argument("--allow-unauthenticated", action="store_true",
                    help="listen on a non-loopback address without a token")
    ap.add_argument("--hold", type=float, default=30.0, help="長輪詢の保留時間の上限（秒）")
    ap.add_argument("--watch-interval", type=float, default=0.5)
    args = ap.parse_args()
    token = None
    if args.token_file:
        with open(args.token_file) as f:
            token = f.read().strip()
        if not token:
            ap.error("the token file is empty")
    node_tokens = load_node_tokens(args.node_tokens) if args.node_tokens else []
    if token is not None and node_tokens:
        ap.error("use either --token-file or --node-tokens, not both")
    host, port = args.listen.rsplit(":", 1)
    host = host.strip("[]")
    # B-470: トークン無しの待ち受けは、明示しない限りループバックだけ
    if token is None and not node_tokens and not _is_loopback(host) and not args.allow_unauthenticated:
        ap.error(f"refusing to listen on {args.listen} without --token-file or --node-tokens "
                 "(pass --allow-unauthenticated to serve any client)")
    store = Store(args.snapshot)

    def watch():
        while True:
            time.sleep(args.watch_interval)
            try:
                store.reload()
            except Exception as e:  # noqa: BLE001 監視は止めない
                print(f"[vds] reload error: {e}", file=sys.stderr, flush=True)

    threading.Thread(target=watch, daemon=True).start()
    srv = ThreadingHTTPServer((host, int(port)), make_handler(store, token, args.hold, node_tokens))
    srv.daemon_threads = True
    print(f"[vds] listening on {args.listen}", flush=True)
    try:
        srv.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
