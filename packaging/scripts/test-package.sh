#!/usr/bin/env bash
# F-251: deb / rpm を systemd の動くコンテナで入れて、既定の設定で動くか・上げる・消すを確かめる。
#
#   packaging/scripts/test-package.sh <deb|rpm> <base-image> <package-file | --repo SETUP_SCRIPT>
#
#   --repo SETUP_SCRIPT: パッケージのファイルではなく、リポジトリから入れる（SETUP_SCRIPT はコンテナの中で
#   リポジトリを足すシェルの断片。F-251 の apt・dnf のリポジトリの試験）。
#
# 確かめること:
#   1. 新しく入れる: deb は入れただけで有効・起動、rpm は enable --now の後に起動。/・/quickstart.html・h2c が 200、
#      アクセスログが journald に出る、既定の設定が /etc/veil/config.toml（root:veil 0640）
#   2. 設定を書き換えて入れ直す（上げるのと同じ扱い）: 書き換えが残り、サービスが動いている
#   3. 消す: サービスが止まる。deb の purge で設定が消える
#   4. 前の版の /var/etc/veil/config.toml がある状態で入れる: それを使い続ける（VEIL_CONFIG）
set -euo pipefail

KIND="${1:?deb|rpm}"; BASE="${2:?base image}"; SRC="${3:?package file or --repo}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TAG="veil-pkgtest:$(echo "$BASE" | tr ':/' '--')"
NAME="veil-pkgtest-$$"
FAILS=0
ok() { echo "ok   $*"; }
bad() { echo "FAIL $*"; echo "::error title=package test ($KIND $BASE)::$*"; FAILS=$((FAILS+1)); }

docker build -q -t "$TAG" --build-arg BASE="$BASE" -f "$SCRIPT_DIR/docker/Dockerfile.test-systemd" "$SCRIPT_DIR/docker" >/dev/null

start_box() {
    docker rm -f "$NAME" >/dev/null 2>&1 || true
    docker run -d --privileged --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup:rw --name "$NAME" "$TAG" >/dev/null
    for _ in $(seq 1 60); do
        docker exec "$NAME" systemctl is-system-running --wait 2>/dev/null | grep -qE 'running|degraded' && return 0
        sleep 1
    done
}
trap 'docker rm -f "$NAME" >/dev/null 2>&1 || true' EXIT
x() { docker exec "$NAME" bash -c "$1"; }

install_pkg() {
    if [ "$SRC" = --repo ]; then
        x "$REPO_SETUP"
        if [ "$KIND" = deb ]; then x 'DEBIAN_FRONTEND=noninteractive apt-get install -y -qq veil' ; else x 'dnf install -y -q veil'; fi
    else
        docker cp "$SRC" "$NAME:/root/veil.$KIND"
        if [ "$KIND" = deb ]; then x 'DEBIAN_FRONTEND=noninteractive apt-get install -y -qq -o Dpkg::Options::=--force-confold /root/veil.deb'
        else x 'dnf install -y -q /root/veil.rpm'; fi
    fi
}
[ "$SRC" = --repo ] && REPO_SETUP="$(cat "${4:?setup script}")"

wait_active() {
    for _ in $(seq 1 30); do x 'systemctl is-active -q veil' && return 0; sleep 1; done
    return 1
}
check_serving() {
    local label="$1"
    if wait_active; then ok "$label: service active"; else bad "$label: service not active"; x 'journalctl -u veil --no-pager -n 40' || true; return; fi
    local code
    for p in / /quickstart.html; do
        code="$(x "curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8080$p" || true)"
        [ "$code" = 200 ] && ok "$label: GET $p" || bad "$label: GET $p -> $code"
    done
    code="$(x "curl -s --http2-prior-knowledge -o /dev/null -w '%{http_code} %{http_version}' http://127.0.0.1:8080/" || true)"
    [ "$code" = "200 2" ] && ok "$label: h2c" || bad "$label: h2c -> $code"
    # ログは非同期に書き出されるので少し待つ
    local seen=0
    for _ in $(seq 1 10); do
        if x 'journalctl -u veil --no-pager | grep -q "type=access.*path=/quickstart.html"'; then seen=1; break; fi
        sleep 1
    done
    [ "$seen" = 1 ] && ok "$label: access log in journald" || bad "$label: no access log"
}

# 1. 新しく入れる
start_box
install_pkg
if [ "$KIND" = deb ]; then
    x 'systemctl is-enabled -q veil' && ok "deb: enabled on install" || bad "deb: not enabled on install"
else
    x 'systemctl is-active -q veil' && bad "rpm: started on install (should not)" || ok "rpm: not started on install"
    x 'systemctl enable --now veil'
fi
check_serving "fresh install"
perm="$(x 'stat -c "%U:%G %a" /etc/veil/config.toml')"
[ "$perm" = "root:veil 640" ] && ok "config owner/mode $perm" || bad "config owner/mode $perm (want root:veil 640)"
x 'journalctl -u veil --no-pager | grep -q "Config File: /etc/veil/config.toml (from default path)"' && ok "reads the default path" || bad "did not read /etc/veil/config.toml from the default path"
x 'ps -o user= -C veil | sort -u | grep -qx veil' && ok "runs as the veil user after dropping privileges" || bad "not running as veil"

# 2. 書き換えて入れ直す
x 'sed -i "s/^level = \"info\"/level = \"info\"  # edited/" /etc/veil/config.toml'
install_pkg
if [ "$KIND" = deb ]; then x 'DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --reinstall -o Dpkg::Options::=--force-confold '"$([ "$SRC" = --repo ] && echo veil || echo /root/veil.deb)"
else x "dnf reinstall -y -q $([ "$SRC" = --repo ] && echo veil || echo /root/veil.rpm)"; fi
x 'grep -q "# edited" /etc/veil/config.toml' && ok "reinstall keeps the edited configuration" || bad "reinstall overwrote the configuration"
check_serving "after reinstall"

# 3. 消す
if [ "$KIND" = deb ]; then
    x 'DEBIAN_FRONTEND=noninteractive apt-get remove -y -qq veil'
    x 'systemctl is-active -q veil' && bad "remove: still running" || ok "remove: stopped"
    x 'test -f /etc/veil/config.toml' && ok "remove keeps the configuration" || bad "remove deleted the configuration"
    x 'DEBIAN_FRONTEND=noninteractive apt-get purge -y -qq veil'
    x 'test ! -e /etc/veil/config.toml' && ok "purge deletes the configuration" || bad "purge left /etc/veil/config.toml"
else
    x 'dnf remove -y -q veil'
    x 'systemctl is-active -q veil' && bad "remove: still running" || ok "remove: stopped"
fi

# 4. 前の版（/var/etc/veil）の利用者
start_box
x 'mkdir -p /var/etc/veil /srv/old && echo "<h1>old config</h1>" > /srv/old/index.html && printf "[server]\nlisten_cleartext = \"127.0.0.1:8080\"\nenable_secure_redirect = false\n[[route]]\n[route.conditions]\npath = \"/\"\n[route.action]\ntype = \"File\"\npath = \"/srv/old/\"\n" > /var/etc/veil/config.toml'
install_pkg
[ "$KIND" = rpm ] && x 'systemctl enable --now veil'
wait_active || true
body="$(x 'curl -s http://127.0.0.1:8080/' || true)"
grep -q 'old config' <<<"$body" && ok "migration: keeps using /var/etc/veil/config.toml" || bad "migration: did not use /var/etc/veil/config.toml"

if [ "$FAILS" -gt 0 ]; then echo "PACKAGE TEST ($KIND $BASE): $FAILS failure(s)"; exit 1; fi
echo "PACKAGE TEST ($KIND $BASE): ALL OK"
