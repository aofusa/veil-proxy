#!/usr/bin/env bash
# Veil パフォーマンス計測用の config.toml バリアントを生成する。
#
# full features ビルドのイメージ (veil:glibc / veil:musl) に対し、
# http2 / ktls / reuseport balancing / open_file_cache の 4 因子を **完全直交（2^4=16）**
# で組み合わせる。どのバリアントも同一の静的ファイル (/var/www/index.html) を File アクションで配信する。
#
# バリアント名: h2_<0|1>_ktls_<0|1>_lb_<cbpf|kernel>_ofc_<0|1>
#   （run_perf.sh は名前の "h2_1" から HTTP/2 負荷の要否を判定する）
#
# 使い方: bash tools/perf/gen_configs.sh
#   環境変数 KEEP_EXISTING=1 で既存 *.toml を消さずに追記する（既定は再生成前にクリア）。
set -euo pipefail

OUT="$(cd "$(dirname "$0")" && pwd)/configs"
mkdir -p "$OUT"

# 0/1 -> false/true（TOML bool へ）
b() { [ "$1" = 1 ] && echo true || echo false; }
# true -> True / false -> False（コメント表記）
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

# 既存の生成物をクリア（_debug*.toml は .gitignore 対象・保持）
if [ "${KEEP_EXISTING:-0}" != 1 ]; then
    find "$OUT" -maxdepth 1 -name '*.toml' ! -name '_debug*.toml' -delete 2>/dev/null || true
fi

# 完全直交 2^4=16: http2 × ktls × lb(cbpf/kernel) × open_file_cache
count=0
for h2 in 0 1; do
    for ktls in 0 1; do
        for lb in cbpf kernel; do
            for ofc in 0 1; do
                name="h2_${h2}_ktls_${ktls}_lb_${lb}_ofc_${ofc}"
                path="$OUT/${name}.toml"
                cfg "$(b "$h2")" "$(b "$ktls")" "$lb" "$(b "$ofc")" > "$path"
                echo "wrote $path"
                count=$((count + 1))
            done
        done
    done
done
echo "生成完了: ${count} バリアント -> $OUT"
