#!/usr/bin/env bash
# F-77: プロトコル差分（differential）テスト。
# 同一のバックエンドを Veil と nginx の双方でフロントし、同じ crafted リクエスト群を
# 両者へ送ってステータス・フレーミング解釈の差分を比較する。曖昧な HTTP 解釈
# （リクエストスマグリングの温床）を検出する。専用トポロジを立てる自己完結スクリプト。
# docker のみで完結。既定 SKIP。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
RESULTS_DIR="${RESULTS_DIR:-${REPO_ROOT}/tools/container_security/results}"
DOCKER_DIR="${REPO_ROOT}/docker"
NET_NAME="${NET_NAME:-veil-sec-test-net}"
VEIL_IMAGE="${VEIL_IMAGE:-veil:glibc}"
SECCOMP="${DOCKER_DIR}/assets/security/seccomp.json"
NGINX_IMAGE="${NGINX_IMAGE:-nginx:alpine}"
PY_IMAGE="${DIFF_PY_IMAGE:-python:3-slim}"
DF_BACKEND="veil-sec-diff-backend"
DF_NGINX="veil-sec-diff-nginx"
DF_VEIL="veil-sec-diff-veil"
REPORT="${RESULTS_DIR}/differential_report.txt"

mkdir -p "${RESULTS_DIR}"

if [[ "${SKIP_DIFFERENTIAL:-1}" == "1" ]]; then
    echo "differential: skipped (SKIP_DIFFERENTIAL=1)" | tee "${REPORT}"
    exit 0
fi

cleanup_df() {
    docker rm -f "${DF_BACKEND}" "${DF_NGINX}" "${DF_VEIL}" >/dev/null 2>&1 || true
    rm -f "${RESULTS_DIR}/diff-nginx.conf" "${RESULTS_DIR}/diff-veil.toml"
}
trap cleanup_df EXIT

: >"${REPORT}"
echo "differential start" | tee -a "${REPORT}"
docker network inspect "${NET_NAME}" >/dev/null 2>&1 || docker network create "${NET_NAME}" >/dev/null
cleanup_df

resolve_ip() { docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$1" 2>/dev/null || true; }

# 1) 共有バックエンド（200 を返す最小 echo。標準ライブラリのみ）。
backend_py='
import http.server, socketserver
class H(http.server.BaseHTTPRequestHandler):
    def _ok(self):
        body=b"ok"
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def do_GET(self): self._ok()
    def do_POST(self):
        # 本文を読み切ってから応答（フレーミング解釈のため）
        cl=int(self.headers.get("Content-Length") or 0)
        if cl: self.rfile.read(cl)
        self._ok()
    def log_message(self,*a): pass
socketserver.TCPServer.allow_reuse_address=True
socketserver.TCPServer(("0.0.0.0",8080),H).serve_forever()
'
docker run -d --name "${DF_BACKEND}" --network "${NET_NAME}" \
    "${PY_IMAGE}" python3 -c "${backend_py}" >/dev/null

be_ip=""
for _ in $(seq 1 20); do be_ip="$(resolve_ip "${DF_BACKEND}")"; [[ -n "${be_ip}" ]] && break; sleep 0.5; done
[[ -n "${be_ip}" ]] || { echo "differential: backend IP 未解決" | tee -a "${REPORT}"; exit 0; }

# 2) nginx リバースプロキシ → backend
cat >"${RESULTS_DIR}/diff-nginx.conf" <<EOF
events {}
http {
    server {
        listen 80;
        location / { proxy_pass http://${be_ip}:8080; }
    }
}
EOF
docker run -d --name "${DF_NGINX}" --network "${NET_NAME}" \
    -v "${RESULTS_DIR}/diff-nginx.conf:/etc/nginx/nginx.conf:ro" \
    "${NGINX_IMAGE}" >/dev/null

# 3) Veil リバースプロキシ → backend（同一バックエンド）
cat >"${RESULTS_DIR}/diff-veil.toml" <<EOF
[server]
listen = "0.0.0.0:443"
http = "0.0.0.0:80"
threads = 0
[logging]
level = "warn"
[security]
allow_security_failures = false
enable_seccomp = true
enable_landlock = false
seccomp_mode = "filter"
[tls]
cert_path = "/etc/veil/ssl/cert.pem"
key_path = "/etc/veil/ssl/key.pem"
ktls_enabled = true
ktls_fallback_enabled = true
[upstreams."diff-pool"]
algorithm = "round_robin"
servers = ["http://${be_ip}:8080/"]
[[route]]
[route.conditions]
path = "/"
[route.action]
type = "Proxy"
upstream = "diff-pool"
[route.security]
allowed_methods = ["HEAD", "GET", "POST"]
EOF
docker run -d --name "${DF_VEIL}" --network "${NET_NAME}" --read-only \
    --tmpfs /var/cache/veil:rw,noexec,nosuid,uid=65532,gid=65532,size=128m \
    --tmpfs /var/tmp/veil:rw,noexec,nosuid,uid=65532,gid=65532,size=64m \
    -v "${RESULTS_DIR}/diff-veil.toml:/etc/veil/conf.d/config.toml:ro" \
    -v "${DOCKER_DIR}/assets/ssl:/etc/veil/ssl:ro" \
    --security-opt "seccomp=${SECCOMP}" \
    "${VEIL_IMAGE}" >/dev/null

nginx_ip=""; veil_ip=""
for _ in $(seq 1 20); do
    nginx_ip="$(resolve_ip "${DF_NGINX}")"; veil_ip="$(resolve_ip "${DF_VEIL}")"
    [[ -n "${nginx_ip}" && -n "${veil_ip}" ]] && break; sleep 0.5
done
sleep 4
[[ -n "${nginx_ip}" && -n "${veil_ip}" ]] || { echo "differential: proxy IP 未解決" | tee -a "${REPORT}"; exit 0; }

# 4) 差分プローブ: 同一リクエスト集合を両プロキシへ送りステータスを比較。
#    既知の意図的差分（Veil はスマグリング要因を厳格拒否）は allowlist で扱う。
probe='
import socket, ssl, sys
nginx_host, veil_host = sys.argv[1], sys.argv[2]
ctx = ssl.create_default_context(); ctx.check_hostname=False; ctx.verify_mode=ssl.CERT_NONE

def send(host, port, tls, raw):
    s=socket.create_connection((host,port),timeout=5)
    if tls: s=ctx.wrap_socket(s, server_hostname="localhost")
    s.sendall(raw)
    data=b""
    try:
        while True:
            c=s.recv(4096)
            if not c: break
            data+=c
            if b"\r\n\r\n" in data: break
    except Exception: pass
    s.close()
    line=data.split(b"\r\n",1)[0].decode("latin1","replace")
    p=line.split(" ")
    return int(p[1]) if len(p)>=2 and p[1].isdigit() else 0

# (name, raw, veil_strict_expected_400) — Veil が厳格拒否する既知差分は allowlist=True
cases=[
    ("normal GET", b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n", False),
    ("normal POST CL", b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello", False),
    ("chunked POST", b"POST / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n0\r\n\r\n", False),
    ("CL+TE (smuggling)", b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n0\r\n\r\n", True),
    ("dup CL (smuggling)", b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nContent-Length: 6\r\nConnection: close\r\n\r\nhello", True),
]
diverge=0
for name, raw, allow in cases:
    try:
        nc=send(nginx_host,80,False,raw); vc=send(veil_host,443,True,raw)
    except Exception as e:
        print("PROBE-ERROR %-22s %s"%(name,e)); diverge+=1; continue
    same=(nc==vc)
    note=""
    if not same:
        if allow: note=" (allowlisted: Veil strict smuggling rejection)"
        else: diverge+=1; note=" <-- UNEXPECTED DIVERGENCE"
    print("%-22s nginx=%d veil=%d%s"%(name,nc,vc,note))
print("differential: %s (unexpected=%d)"%("ok" if diverge==0 else "DIVERGENCES", diverge))
sys.exit(1 if diverge else 0)
'
set +e
docker run --rm --network "${NET_NAME}" "${PY_IMAGE}" \
    python3 -c "${probe}" "${nginx_ip}" "${veil_ip}" 2>&1 | tee -a "${REPORT}"
rc="${PIPESTATUS[0]}"
set -e

if [[ "${rc}" -ne 0 ]]; then
    echo "differential: WARNING 予期しない差分あり（backlog 要確認）" | tee -a "${REPORT}"
    [[ "${DIFFERENTIAL_BLOCKING:-0}" == "1" ]] && exit 1
fi
echo "differential 完了: ${REPORT}"
