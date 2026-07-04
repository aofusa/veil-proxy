#!/usr/bin/env bash
# Veil パフォーマンス計測用の config.toml バリアントを生成する。
#
# full features ビルドのイメージ (veil:glibc / veil:musl) に対し、
# ktls / http2 / reuseport balancing / open_file_cache が効く設定のみを組み合わせる。
# どのバリアントも同一の静的ファイル (/var/www/index.html) を File アクションで配信する。
#
# 使い方: bash tools/perf/gen_configs.sh
set -euo pipefail

OUT="$(cd "$(dirname "$0")" && pwd)/configs"
mkdir -p "$OUT"

# true -> True / false -> False（コメント表記を Python 版と揃える）
cap() { case "$1" in true) echo True;; false) echo False;; *) echo "$1";; esac; }

# cfg <http2> <ktls> <lb> <ofc>  -> config.toml 本文を標準出力へ
cfg() {
    local http2="$1" ktls="$2" lb="$3" ofc="$4"
    local ofc_section=""
    if [ "$ofc" = "true" ]; then
        ofc_section=$'[route.open_file_cache]\nenabled = true\nvalid_duration_secs = 60\nmax_entries = 10000\n'
    fi
    cat <<EOF
# 自動生成: Veil パフォーマンス計測用バリアント
# http2=$(cap "$http2") ktls=$(cap "$ktls") lb=${lb} open_file_cache=$(cap "$ofc")
[server]
listen = "0.0.0.0:443"
http = "0.0.0.0:80"
http2_enabled = ${http2}
threads = 0

[logging]
# アクセスログ (INFO) はホットパスのオーバーヘッドになるため計測中は抑止（nginx も access_log off で公平化）
level = "warn"

[security]
allow_security_failures = false
drop_privileges_user = "nonroot"
drop_privileges_group = "nonroot"
enable_seccomp = true
enable_landlock = true
enable_sandbox = false
seccomp_mode = "filter"
landlock_read_paths = ["/var/www", "/var/cache/veil", "/var/tmp/veil"]
landlock_write_paths = ["/var/cache/veil", "/var/tmp/veil"]

[performance]
huge_pages_enabled = false
reuseport_balancing = "${lb}"

[tls]
cert_path = "/etc/veil/ssl/cert.pem"
key_path = "/etc/veil/ssl/key.pem"
ktls_enabled = ${ktls}
ktls_fallback_enabled = true

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/var/www/"
[route.security]
allowed_methods = ["HEAD", "GET"]
EOF
    printf '%s' "$ofc_section"
}

# バリアント定義: 名前  http2  ktls  lb  ofc
VARIANTS=(
    "base        true  true  cbpf   false"
    "no_ktls     true  false cbpf   false"
    "no_http2    false true  cbpf   false"
    "kernel_lb   true  true  kernel false"
    "ofc         true  true  cbpf   true"
    "no_ktls_ofc true  false cbpf   true"
)

for v in "${VARIANTS[@]}"; do
    # shellcheck disable=SC2086
    set -- $v
    name="$1" http2="$2" ktls="$3" lb="$4" ofc="$5"
    path="$OUT/${name}.toml"
    cfg "$http2" "$ktls" "$lb" "$ofc" > "$path"
    echo "wrote $path"
done
