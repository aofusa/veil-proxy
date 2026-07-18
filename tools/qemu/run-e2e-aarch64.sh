#!/usr/bin/env bash
#
# aarch64 io_uring E2E スモーク（F-128）
# =====================================
#
# ホストで veil を aarch64-unknown-linux-gnu にクロスビルドし、tools/qemu の
# full-system QEMU VM（実 aarch64 カーネル）へ転送して **io_uring バックエンド**で
# 起動し、HTTPS 静的配信のスモーク E2E を実行する。
#
# 前提: `tools/qemu/aarch64-vm.sh setup && up && wait` で VM が起動済みであること
#       （本スクリプトは wait を内部で呼ぶので up まで済んでいれば良い）。
#
# 使い方:
#   tools/qemu/run-e2e-aarch64.sh
#
# 環境変数:
#   CARGO_FEATURES  クロスビルドする feature（default: 既定 = io_uring + ktls + http2 等）
#   VEIL_PORT       VM 内で veil が待ち受ける HTTPS ポート（default: 8443）
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
VM="${SCRIPT_DIR}/aarch64-vm.sh"
RUST_TARGET="aarch64-unknown-linux-gnu"
VEIL_PORT="${VEIL_PORT:-8443}"
STAGE="${ROOT}/target/aarch64-e2e-stage"

log() { echo "[aarch64-e2e] $*" >&2; }
die() { echo "[aarch64-e2e] ERROR: $*" >&2; exit 1; }

# 1) クロスビルド（io_uring デフォルト。cargo-zigbuild で aarch64-gnu、Linux ELF は
#    aws-lc-sys アセンブリも問題なくリンクできる）
log "veil を ${RUST_TARGET} にクロスビルド（io_uring デフォルト）"
if [[ -n "${CARGO_FEATURES:-}" ]]; then
    FEAT_ARGS=(--features "${CARGO_FEATURES}")
else
    FEAT_ARGS=()  # 既定 feature（ktls,http2,mimalloc = io_uring）
fi
docker run --rm -v "${ROOT}:/io" -w /io messense/cargo-zigbuild \
    cargo zigbuild --release --target "${RUST_TARGET}" "${FEAT_ARGS[@]}"

BIN="${ROOT}/target/${RUST_TARGET}/release/veil"
[[ -f "${BIN}" ]] || die "ビルド成果物が見つかりません: ${BIN}"
file "${BIN}" 2>/dev/null | grep -q 'aarch64\|ARM aarch64' || log "警告: file 判定で aarch64 を確認できません"

# 2) 証明書 + config + 静的コンテンツを生成
log "テスト証明書 / config / コンテンツを生成"
rm -rf "${STAGE}"; mkdir -p "${STAGE}/www"
openssl req -x509 -newkey EC -pkeyopt ec_paramgen_curve:secp384r1 -nodes \
    -keyout "${STAGE}/key.pem" -out "${STAGE}/cert.pem" \
    -days 3 -subj "/CN=localhost" >/dev/null 2>&1
echo "veil-aarch64-iouring-ok" > "${STAGE}/www/index.html"
cat > "${STAGE}/veil.toml" <<EOF
[server]
listen = "127.0.0.1:${VEIL_PORT}"
threads = 2

[tls]
cert_path = "/home/veil/e2e/cert.pem"
key_path = "/home/veil/e2e/key.pem"

[logging]
level = "info"

[[route]]
[route.conditions]
host = "localhost"
path = "/*"
[route.action]
type = "File"
path = "/home/veil/e2e/www"
index = "index.html"
EOF

# 3) VM が SSH 到達するまで待ち、資材を転送
"${VM}" wait
log "VM へ資材を転送"
"${VM}" ssh 'rm -rf ~/e2e && mkdir -p ~/e2e'
for f in veil.toml cert.pem key.pem; do "${VM}" scp "${STAGE}/${f}" "/home/veil/e2e/${f}"; done
"${VM}" ssh 'mkdir -p ~/e2e/www'
"${VM}" scp "${STAGE}/www/index.html" "/home/veil/e2e/www/index.html"
"${VM}" scp "${BIN}" "/home/veil/e2e/veil"
"${VM}" ssh 'chmod +x ~/e2e/veil'

# 4) io_uring 可用性を確認して veil を起動 → HTTPS スモーク
log "VM で io_uring 可用性を確認して veil を起動"
"${VM}" ssh "bash -s" <<REMOTE
set -e
echo "--- guest arch/kernel ---"; uname -mr
echo "--- io_uring_disabled ---"; cat /proc/sys/kernel/io_uring_disabled 2>/dev/null || echo "none(=enabled)"
echo "--- binary ---"; file ~/e2e/veil | head -1
pkill -x veil 2>/dev/null || true; sleep 1
cd ~/e2e
nohup ./veil --config veil.toml > veil.log 2>&1 &
sleep 6
echo "--- veil startup log (backend line) ---"
grep -iE 'io_uring|reactor|backend|Worker started|listen' veil.log | head -10 || true
echo "--- HTTPS smoke (curl -k) ---"
code=\$(curl -sk -o /tmp/body.txt -w '%{http_code}' https://localhost:${VEIL_PORT}/ || echo 000)
echo "HTTP_CODE=\$code"; echo "BODY=\$(cat /tmp/body.txt)"
if [ "\$code" = "200" ] && grep -q 'veil-aarch64-iouring-ok' /tmp/body.txt; then
  echo "E2E_RESULT=PASS"
else
  echo "E2E_RESULT=FAIL"; echo "--- full veil.log ---"; cat veil.log
fi
pkill -x veil 2>/dev/null || true
REMOTE

log "完了（上の E2E_RESULT を確認）"
