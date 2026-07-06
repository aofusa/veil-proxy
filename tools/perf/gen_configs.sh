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
# ============================================================
# full features 機能ショーケース（default=ktls/http2 に加えて full で有効化される
# compression / cache / buffering / 逆プロキシ経路のスループットを計測する）。
# いずれも最良の基盤（http2=on, ktls=on, kernel LB）上に 1 機能を重ねる。
# 名前を h2_1_* にして run_perf.sh が HTTP/2 負荷も実施するようにする。
# proxy / buffering は上流 backend（run_perf.sh が起動する perf-backend）へ中継する。
# ============================================================

feat_base_head() {
    cat <<'EOF'
[server]
listen = "0.0.0.0:443"
http = "0.0.0.0:80"
http2_enabled = true
threads = 0

[logging]
level = "warn"

[security]
allow_security_failures = false
drop_privileges_user = "nonroot"
drop_privileges_group = "nonroot"
enable_seccomp = true
enable_landlock = true
enable_sandbox = false
seccomp_mode = "filter"
landlock_read_paths = ["/var/www", "/var/cache/veil", "/var/tmp/veil", "/etc/veil", "/etc/resolv.conf", "/etc/hosts", "/etc/nsswitch.conf", "/lib", "/lib64", "/usr"]
landlock_write_paths = ["/var/cache/veil", "/var/tmp/veil"]

[performance]
huge_pages_enabled = false
reuseport_balancing = "kernel"

[tls]
cert_path = "/etc/veil/ssl/cert.pem"
key_path = "/etc/veil/ssl/key.pem"
ktls_enabled = true
ktls_fallback_enabled = true
EOF
}

# 逆プロキシ系（proxy / buffering）が中継する上流定義。
feat_upstream() {
    cat <<'EOF'

[upstreams."perf-backend"]
algorithm = "round_robin"
servers = ["http://perf-backend:80/"]
EOF
}

# compression: 静的ファイル + レスポンス圧縮（Accept-Encoding 時に zstd/br/gzip）
{
    feat_base_head
    cat <<'EOF'

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/var/www/"
[route.compression]
enabled = true
preferred_encodings = ["zstd", "br", "gzip"]
min_size = 256
[route.security]
allowed_methods = ["HEAD", "GET"]
EOF
} > "$OUT/h2_1_feat_compression.toml"
echo "wrote $OUT/h2_1_feat_compression.toml"
count=$((count + 1))

# cache: 静的ファイル + インメモリキャッシュ（GET/200）
{
    feat_base_head
    cat <<'EOF'

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/var/www/"
[route.cache]
enabled = true
default_ttl_secs = 60
methods = ["GET"]
cacheable_statuses = [200]
[route.security]
allowed_methods = ["HEAD", "GET"]
EOF
} > "$OUT/h2_1_feat_cache.toml"
echo "wrote $OUT/h2_1_feat_cache.toml"
count=$((count + 1))

# proxy: 逆プロキシ経路（backend へ中継。プロキシのホットパス）
{
    feat_base_head
    feat_upstream
    cat <<'EOF'

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "Proxy"
upstream = "perf-backend"
[route.security]
allowed_methods = ["HEAD", "GET"]
EOF
} > "$OUT/h2_1_feat_proxy.toml"
echo "wrote $OUT/h2_1_feat_proxy.toml"
count=$((count + 1))

# buffering: 逆プロキシ + 高度なバッファリング制御（full buffering）
{
    feat_base_head
    feat_upstream
    cat <<'EOF'

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "Proxy"
upstream = "perf-backend"
[route.buffering]
mode = "full"
[route.security]
allowed_methods = ["HEAD", "GET"]
EOF
} > "$OUT/h2_1_feat_buffering.toml"
echo "wrote $OUT/h2_1_feat_buffering.toml"
count=$((count + 1))

echo "生成完了: ${count} バリアント -> $OUT"
