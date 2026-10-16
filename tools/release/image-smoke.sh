#!/usr/bin/env bash
# F-237: 公開するコンテナイメージの起動の試験。
#
#   tools/release/image-smoke.sh <image> [expect]
#
# expect（既定 auto）:
#   epoll  … Docker の既定の seccomp で起動し、io_uring 版から epoll 版へ切り替わることを確かめる
#   uring  … seccomp を外して起動し、io_uring のまま動くことを確かめる
#   auto   … 両方（既定の seccomp が io_uring を許すホストでは epoll の確認を飛ばす）
#
# どちらの起動でも HTTP/1.1（平文）・HTTP/2（TLS）・HTTP/3（UDP）で静的ファイルの応答を確かめ、
# `--version` がワークスペースの版と一致するかを見る。HTTP/3 のクライアントは H3_CURL_IMAGE の curl。
# snap の docker は /tmp を読めないので、作業ディレクトリは $HOME の下に作る（SMOKE_DIR で変更可）。
set -euo pipefail

IMAGE="${1:?usage: $0 <image> [epoll|uring|auto]}"
EXPECT="${2:-auto}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
H3_CURL_IMAGE="${H3_CURL_IMAGE:-alpine/curl-http3:latest}"
WORK="${SMOKE_DIR:-$HOME/.cache/veil-image-smoke}/$$"
mkdir -p "$WORK"
NAME="veil-smoke-$$"
fail=0
cleanup() { docker rm -f "$NAME" >/dev/null 2>&1 || true; rm -rf "$WORK"; }
trap cleanup EXIT

ok() { echo "ok   $*"; }
bad() { echo "FAIL $*"; fail=1; }

VERSION="$(awk '/^\[workspace.package\]/{w=1;next} /^\[/{w=0} w && /^version *=/{gsub(/[" ]/,"");sub(/version=/,"");print;exit}' "$ROOT/Cargo.toml")"

openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 2 -subj /CN=localhost \
    -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" -keyout "$WORK/key.pem" -out "$WORK/cert.pem" 2>/dev/null
chmod 644 "$WORK/key.pem" "$WORK/cert.pem"
cat > "$WORK/config.toml" <<CFG
[server]
listen = "0.0.0.0:443"
listen_cleartext = "0.0.0.0:80"
enable_secure_redirect = false
http2_enabled = true
threads = 2

[logging]
level = "info"

[security]
drop_privileges_user = "nonroot"
drop_privileges_group = "nonroot"
enable_seccomp = true
enable_landlock = true
seccomp_mode = "filter"
landlock_read_paths = ["/var/www", "/etc/veil", "/var/cache/veil", "/var/tmp/veil"]
landlock_write_paths = ["/var/cache/veil", "/var/tmp/veil"]

[tls]
cert_path = "/etc/veil/ssl/cert.pem"
key_path = "/etc/veil/ssl/key.pem"
ktls_enabled = false

[http3]
enabled = true

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/var/www/"
CFG

run_case() { # run_case <label> <docker run の追加引数...>
    local label="$1"; shift
    docker rm -f "$NAME" >/dev/null 2>&1 || true
    docker run -d --name "$NAME" "$@" \
        -p 127.0.0.1:18080:80 -p 127.0.0.1:18443:443 -p 127.0.0.1:18443:443/udp \
        -v "$WORK/config.toml:/etc/veil/conf.d/config.toml:ro" \
        -v "$WORK/cert.pem:/etc/veil/ssl/cert.pem:ro" -v "$WORK/key.pem:/etc/veil/ssl/key.pem:ro" \
        "$IMAGE" >/dev/null
    for _ in $(seq 1 50); do
        curl -fsS -o /dev/null http://127.0.0.1:18080/ 2>/dev/null && break
        sleep 0.2
    done
    local logs; logs="$(docker logs "$NAME" 2>&1 || true)"
    if [ "$label" = epoll ]; then
        if grep -q "Switching to the epoll build" <<<"$logs"; then ok "$label: switched to /veil.epoll"; else bad "$label: no switch to the epoll build"; fi
    else
        if grep -q "Switching to the epoll build" <<<"$logs"; then bad "$label: switched although io_uring should be allowed"; else ok "$label: stayed on io_uring"; fi
    fi
    if curl -fsS http://127.0.0.1:18080/ -o /dev/null; then ok "$label: HTTP/1.1"; else bad "$label: HTTP/1.1"; echo "$logs" | tail -20; fi
    local v; v="$(curl -ksS --http2 -o /dev/null -w '%{http_version}' https://127.0.0.1:18443/ || true)"
    if [ "$v" = 2 ]; then ok "$label: HTTP/2"; else bad "$label: HTTP/2 (got '$v')"; fi
    v="$(docker run --rm --network host "$H3_CURL_IMAGE" -ksS --http3-only -o /dev/null -w '%{http_version}' https://127.0.0.1:18443/ 2>&1 || true)"
    if [ "$v" = 3 ]; then ok "$label: HTTP/3"; else bad "$label: HTTP/3 (got '$v')"; fi
    docker rm -f "$NAME" >/dev/null 2>&1 || true
}

# 既定の seccomp が io_uring を拒否するか（拒否しないホストでは epoll の確認ができない）
uring_blocked=1
if docker run --rm --entrypoint /veil "$IMAGE" --version 2>&1 | grep -q "Switching to the epoll build"; then
    uring_blocked=1
else
    uring_blocked=0
fi

out="$(docker run --rm --security-opt seccomp=unconfined --entrypoint /veil "$IMAGE" --version 2>&1 || true)"
if grep -q "$VERSION" <<<"$out"; then ok "--version is $VERSION"; else bad "--version: '$out' (want $VERSION)"; fi
if docker run --rm --entrypoint /veil.epoll "$IMAGE" --version >/dev/null 2>&1; then ok "/veil.epoll exists"; else bad "/veil.epoll missing"; fi

case "$EXPECT" in
    epoll) run_case epoll ;;
    uring) run_case uring --security-opt seccomp=unconfined ;;
    auto)
        if [ "$uring_blocked" = 1 ]; then run_case epoll; else echo "skip epoll: the default seccomp of this host allows io_uring"; fi
        run_case uring --security-opt seccomp=unconfined
        ;;
    *) echo "unknown expect: $EXPECT" >&2; exit 2 ;;
esac

[ "$fail" = 0 ] && echo "image smoke: all ok" || echo "image smoke: FAILED"
exit "$fail"
