#!/usr/bin/env bash
# F-237: 公開するコンテナイメージの起動の試験。
#
#   tools/release/image-smoke.sh <image> [expect]
#
# expect（既定 auto）:
#   container … 公開イメージ（full-container の /veil の 1 本、/veil.epoll は無い）。io_uring を拒否する seccomp と
#               Docker の既定の seccomp で起動し、そのまま動くこと（io_uring 版への依存も切り替えも無いこと）を確かめる
#   epoll  … io_uring を拒否する seccomp で起動し、io_uring 版から epoll 版へ切り替わることを確かめる
#            （Docker の既定の seccomp が io_uring を許すホスト・版もあるので、拒否するプロファイルを明示する）
#   uring  … seccomp を外して起動し、io_uring のまま動くことを確かめる
#   auto   … 両方
#
# どちらの起動でも HTTP/1.1（平文）・HTTP/2（TLS）・HTTP/3（UDP）で静的ファイルの応答を確かめ、
# `--version` がワークスペースの版と一致するかを見る。HTTP/3 のクライアントは H3_CURL_IMAGE の curl。
# snap の docker は /tmp と $HOME の隠しディレクトリを読めないので、作業ディレクトリは $HOME/veil-image-smoke に作る（SMOKE_DIR で変更可）。
set -euo pipefail

IMAGE="${1:?usage: $0 <image> [container|epoll|uring|auto]}"
EXPECT="${2:-auto}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
H3_CURL_IMAGE="${H3_CURL_IMAGE:-alpine/curl-http3:latest}"
WORK="${SMOKE_DIR:-$HOME/veil-image-smoke}/$$"
mkdir -p "$WORK"
NAME="veil-smoke-$$"
fail=0
cleanup() { docker rm -f "$NAME" >/dev/null 2>&1 || true; rm -rf "$WORK"; }
trap cleanup EXIT

ok() { echo "ok   $*"; }
# GitHub Actions では失敗を注釈にも出す（公開リポジトリの注釈はログイン無しで API から読める）
bad() {
    echo "FAIL $*"; fail=1
    if [ -n "${GITHUB_ACTIONS:-}" ]; then echo "::error title=image smoke::$*"; fi
}
# 失敗したときの手がかり（コンテナのログの最後）を注釈に出す
annotate_logs() {
    [ -n "${GITHUB_ACTIONS:-}" ] || return 0
    # 注釈は 1 手順につき 10 件までなので、状態とログの最後を 1 件にまとめる（改行は %0A）
    local st body
    st="$(docker inspect -f '{{.State.Status}} exit={{.State.ExitCode}} oom={{.State.OOMKilled}}' "$NAME" 2>&1)"
    body="$(tail -40 <<<"$1" | cut -c1-300 | sed 's/%/%25/g' | awk '{printf "%s%%0A", $0}')"
    echo "::warning title=container ($st)::${body}"
}

# HTTP/3 のクライアントのイメージは先に取っておく（取得のメッセージが curl の結果に混ざらないように）
docker pull -q "$H3_CURL_IMAGE" >/dev/null

VERSION="$(awk '/^\[workspace.package\]/{w=1;next} /^\[/{w=0} w && /^version *=/{gsub(/[" ]/,"");sub(/version=/,"");print;exit}' "$ROOT/Cargo.toml")"

# io_uring の 3 つの syscall だけを EPERM にする seccomp（それ以外は許す）
cat > "$WORK/no-uring.json" <<'SECCOMP'
{"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":["io_uring_setup","io_uring_enter","io_uring_register"],"action":"SCMP_ACT_ERRNO","errnoRet":1}]}
SECCOMP

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
    # 起動を最大 30 秒待つ（arm64 のランナーは起動が遅いことがある）
    for _ in $(seq 1 150); do
        curl -fsS -o /dev/null http://127.0.0.1:18080/ 2>/dev/null && break
        sleep 0.2
    done
    local logs; logs="$(docker logs "$NAME" 2>&1 || true)"
    if [ "${label%%-*}" = container ]; then
        if grep -q "Switching to the epoll build" <<<"$logs"; then bad "$label: exec'd a fallback (expected a single epoll build)"; else ok "$label: no fallback exec"; fi
    elif [ "$label" = epoll ]; then
        if grep -q "Switching to the epoll build" <<<"$logs"; then ok "$label: switched to /veil.epoll"; else bad "$label: no switch to the epoll build"; annotate_logs "$logs"; fi
    else
        if grep -q "Switching to the epoll build" <<<"$logs"; then bad "$label: switched although io_uring should be allowed"; else ok "$label: stayed on io_uring"; fi
    fi
    if curl -fsS http://127.0.0.1:18080/ -o /dev/null; then ok "$label: HTTP/1.1"; else bad "$label: HTTP/1.1"; echo "$logs" | tail -20; annotate_logs "$logs"; fi
    local v; v="$(curl -ksS --http2 -o /dev/null -w '%{http_version}' https://127.0.0.1:18443/ || true)"
    if [ "$v" = 2 ]; then ok "$label: HTTP/2"; else bad "$label: HTTP/2 (got '$v')"; fi
    local err
    v="$(docker run --rm --network host --entrypoint curl "$H3_CURL_IMAGE" -ksS --http3-only -o /dev/null -w '%{http_version}' https://127.0.0.1:18443/ 2>"$WORK/h3.err" || true)"
    err="$(tr '\n' ' ' < "$WORK/h3.err")"
    if [ "$v" = 3 ]; then ok "$label: HTTP/3"; else bad "$label: HTTP/3 (got '$v' $err)"; fi
    docker rm -f "$NAME" >/dev/null 2>&1 || true
}

out="$(docker run --rm --security-opt seccomp=unconfined --entrypoint /veil "$IMAGE" --version 2>&1 || true)"
if grep -q "$VERSION" <<<"$out"; then ok "--version is $VERSION"; else bad "--version: '$out' (want $VERSION)"; fi
if [ "$EXPECT" = container ]; then
    if docker run --rm --entrypoint /veil.epoll "$IMAGE" --version >/dev/null 2>&1; then bad "/veil.epoll exists (expected a single full-container build)"; else ok "no /veil.epoll"; fi
elif docker run --rm --entrypoint /veil.epoll "$IMAGE" --version >/dev/null 2>&1; then ok "/veil.epoll exists"; else bad "/veil.epoll missing"; fi

case "$EXPECT" in
    container)
        # io_uring を拒否しても（io_uring 版なら起動できない）、Docker の既定の seccomp でも動くこと
        run_case container-no-uring --security-opt "seccomp=$WORK/no-uring.json"
        run_case container-default
        ;;
    epoll) run_case epoll --security-opt "seccomp=$WORK/no-uring.json" ;;
    uring) run_case uring --security-opt seccomp=unconfined ;;
    auto)
        run_case epoll --security-opt "seccomp=$WORK/no-uring.json"
        run_case uring --security-opt seccomp=unconfined
        ;;
    *) echo "unknown expect: $EXPECT" >&2; exit 2 ;;
esac

[ "$fail" = 0 ] && echo "image smoke: all ok" || echo "image smoke: FAILED"
exit "$fail"
