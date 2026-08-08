#!/usr/bin/env bash
# =============================================================================
# profile_ab.sh — cargo release プロファイル（lto / codegen-units 等）の A/B 計測
# =============================================================================
#
# `[profile.release]` の設定違いでどれだけスループットが変わるかを実測する。
# Cargo.toml を書き換えずに **環境変数 `CARGO_PROFILE_RELEASE_*` で上書き**して
# ビルドし分けるので、比較中もリポジトリはクリーンなまま。
#
# ディスク節約のため単一の target-dir を使い回し、ビルド成果物はバリアント名付きで
# 退避する（詳細は build_variant のコメント）。プロファイル変更は毎回フルリビルドになる。
#
# 使い方（Linux ホスト）:
#   bash tools/perf/profile_ab.sh            # 全バリアントをビルドして計測
#   VARIANTS="baseline lto_fat_cgu1" bash tools/perf/profile_ab.sh
#
# 前提: docker（負荷生成に williamyeh/wrk を使う）、自己署名証明書と配信ファイルを
#       置いた作業ディレクトリ（WORK）。FreeBSD ゲストでは wrk/h2load を直接使う
#       `tools/perf/freebsd/` 側のハーネスを使うこと。
# =============================================================================
set -euo pipefail

REPO="$(cd "$(dirname "$0")/../.." && pwd)"
WORK="${WORK:?WORK（証明書と www/ を置いた作業ディレクトリ）を指定してください}"
CONF="${CONF:-${WORK}/perf.toml}"
PORT="${PORT:-19445}"
FEATURES="${FEATURES:-full}"
DURATION="${DURATION:-8}"
CONNS="${CONNS:-64}"
ROUNDS="${ROUNDS:-2}"

# バリアント定義: 名前 → CARGO_PROFILE_RELEASE_* の設定
# 既定（何も指定しない）は lto=false / codegen-units=16 / panic=unwind。
variant_env() {
    case "$1" in
        baseline)        echo "" ;;
        thin_lto)        echo "CARGO_PROFILE_RELEASE_LTO=thin" ;;
        fat_lto)         echo "CARGO_PROFILE_RELEASE_LTO=fat" ;;
        cgu1)            echo "CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1" ;;
        thin_lto_cgu1)   echo "CARGO_PROFILE_RELEASE_LTO=thin CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1" ;;
        fat_lto_cgu1)    echo "CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1" ;;
        fat_lto_cgu1_abort)
            echo "CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_PROFILE_RELEASE_PANIC=abort" ;;
        *) echo "UNKNOWN" ;;
    esac
}

VARIANTS="${VARIANTS:-baseline thin_lto fat_lto cgu1 thin_lto_cgu1 fat_lto_cgu1}"

log() { printf '[profile_ab] %s\n' "$*" >&2; }

# ディスク節約のため **単一の target-dir** を使い回し、ビルドのたびに成果物を
# ${STASH} へ退避する（バリアントごとに target-dir を分けると 1 個あたり数 GB 必要で
# ディスクが尽きる）。プロファイル変更は毎回フルリビルドになる点は許容する。
STASH="${STASH:-${REPO}/target-prof-bins}"
PROF_TARGET="${PROF_TARGET:-${REPO}/target-prof}"

build_variant() {
    local name="$1" env_str
    env_str="$(variant_env "$name")"
    [[ "$env_str" == "UNKNOWN" ]] && { log "未知のバリアント: $name"; return 1; }
    mkdir -p "$STASH"
    BIN="${STASH}/veil-${name}"

    if [[ -x "$BIN" ]]; then
        log "reuse: ${name}（退避済みバイナリを使用）"
        BUILD_SECS=0
    else
        log "build: ${name} (${env_str:-cargo 既定})"
        local start end
        start=$(date +%s)
        # shellcheck disable=SC2086
        ( cd "$REPO" && env $env_str CARGO_TARGET_DIR="$PROF_TARGET" \
            cargo build --release --features "$FEATURES" >/dev/null 2>&1 )
        end=$(date +%s)
        BUILD_SECS=$((end - start))
        cp "${PROF_TARGET}/release/veil" "$BIN"
    fi
    BIN_BYTES=$(stat -c %s "$BIN" 2>/dev/null || stat -f %z "$BIN")
}

# wrk を docker で実行して req/s を返す
run_wrk() {
    local url="$1"
    docker run --rm --network=host williamyeh/wrk:latest \
        -t2 -c"${CONNS}" -d"${DURATION}s" --timeout 10s "$url" 2>/dev/null \
        | awk '/Requests\/sec:/ {print $2}'
}

measure_variant() {
    local name="$1"
    pkill -x veil >/dev/null 2>&1 || true
    sleep 1
    # **WORK へ cd してから起動する**。計測用 config は証明書を相対パス
    # （cert.pem / key.pem）で指定しているため、リポジトリルートのまま起動すると
    # veil が証明書を開けず即終了し、負荷ツールが空結果を返して
    # `set -o pipefail` でスクリプトごと落ちる（実際に踏んだ）。
    ( cd "$WORK" && "$BIN" -c "$CONF" >/dev/null 2>&1 & )
    sleep 4
    # 起動できたかを明示的に確認する（黙って空結果になるのを防ぐ）。
    if ! curl -sk --max-time 5 -o /dev/null "https://127.0.0.1:${PORT}/small.html"; then
        log "ERROR: ${name} の veil が起動していない（config: ${CONF}）"
        printf '%s\tNA\tNA\t%s\t%s\n' "$name" "$BIN_BYTES" "$BUILD_SECS"
        return 0
    fi
    local large small
    large=$(run_wrk "https://127.0.0.1:${PORT}/index.html")
    sleep 1
    small=$(run_wrk "https://127.0.0.1:${PORT}/small.html")
    pkill -x veil >/dev/null 2>&1 || true
    printf '%s\t%s\t%s\t%s\t%s\n' "$name" "${large:-NA}" "${small:-NA}" "$BIN_BYTES" "$BUILD_SECS"
}

printf 'variant\tlarge_rps\tsmall_rps\tbin_bytes\tbuild_secs\n'
for r in $(seq 1 "$ROUNDS"); do
    for v in $VARIANTS; do
        build_variant "$v"
        measure_variant "$v"
    done
done
pkill -x veil >/dev/null 2>&1 || true
