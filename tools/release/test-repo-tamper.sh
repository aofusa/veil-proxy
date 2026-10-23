#!/usr/bin/env bash
# F-251: 改ざんした索引・パッケージを apt・dnf が拒否することを確かめる。
#
#   tools/release/test-repo-tamper.sh <repo-dir> <port>
#
# <repo-dir> の写しを 2 通りに改ざんして <port> で配り、コンテナから入れようとして失敗することを確かめる。
#   1. apt: Packages の 1 バイトを変える（Release の署名・ハッシュと合わない）
#   2. dnf: rpm を別の内容に差し替える（署名・repodata のハッシュと合わない）
set -uo pipefail
REPO="${1:?repo-dir}"; PORT="${2:?port}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
W="$(mktemp -d "${TMPDIR_FOR_DOCKER:-$HOME}/veil-tamper.XXXXXX")"
FAILS=0
ok() { echo "ok   $*"; }
bad() { echo "FAIL $*"; echo "::error title=repo tamper test::$*"; FAILS=$((FAILS+1)); }
cleanup() { [ -n "${SRV:-}" ] && kill "$SRV" 2>/dev/null; rm -rf "$W"; }
trap cleanup EXIT
cp -a "$REPO" "$W/repo"
URL="http://172.17.0.1:$PORT"
# veil.repo は作ったときの URL を持っているので、改ざんした写しを指すように書き換える
sed -i -E "s#^(baseurl|gpgkey)=https?://[^/]+(/[^ ]*)?/(rpm|veil\.asc)\$#\1=$URL/\3#" "$W/repo/veil.repo"
(cd "$W/repo" && exec python3 -m http.server "$PORT" --bind 0.0.0.0 >/dev/null 2>&1) & SRV=$!
sleep 1

# 1. apt
f="$W/repo/apt/dists/stable/main/binary-amd64/Packages"
sed -i '0,/^Version: /s/^Version: \(.*\)$/Version: \1+tampered/' "$f"; gzip -9nc "$f" > "$f.gz"
out="$(docker run --rm ubuntu:24.04 bash -c "apt-get update -qq >/dev/null; apt-get install -y -qq curl ca-certificates >/dev/null 2>&1
  $(sed "s|VEIL_REPO_URL|$URL|g" "$ROOT/packaging/repos/apt-setup.sh" | grep -v '^set -e')
  apt-get install -y -qq veil" 2>&1)"; rc=$?
if [ "$rc" != 0 ] && grep -qiE 'hash sum mismatch|File has unexpected size|Mirror sync|is not signed|NOSPLIT|signature' <<<"$out"; then ok "apt rejects a tampered Packages index"
else bad "apt accepted a tampered index (rc=$rc): $(tail -3 <<<"$out")"; fi

# 2. dnf
r="$(ls "$W"/repo/rpm/x86_64/*.rpm | head -1)"
printf 'not the package' > "$r"
out="$(docker run --rm fedora:41 bash -c "$(sed "s|VEIL_REPO_URL|$URL|g" "$ROOT/packaging/repos/dnf-setup.sh" | grep -v '^set -e')
  dnf install -y -q veil" 2>&1)"; rc=$?
if [ "$rc" != 0 ]; then ok "dnf rejects a tampered package"; else bad "dnf installed a tampered package"; fi

[ "$FAILS" = 0 ] && echo "REPO TAMPER TEST: ALL OK" || { echo "REPO TAMPER TEST: $FAILS failure(s)"; exit 1; }
