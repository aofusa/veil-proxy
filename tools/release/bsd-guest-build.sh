#!/bin/sh
# F-247: BSD の VM の中（release-bsd の workflow。cross-platform-actions）で、配布用の veil をビルドしてアーカイブを作る。
#
#   sh tools/release/bsd-guest-build.sh <freebsd|openbsd|netbsd> <x86_64|aarch64> <out-dir>
#
# ツールの入れ方・環境変数は tools/qemu/bsd-vm.sh の toolchain・_guest_env_prefix と同じ（理由はそちらのコメント）。
# 作るもの: <out-dir>/veil-<ver>-<arch>-unknown-<os>.tar.gz（stage-archive.sh の中身）と、単体試験・起動の試験の結果。
set -eu

OS="$1"; ARCH="$2"; OUT="$3"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
TARGET="${ARCH}-unknown-${OS}"
mkdir -p "$OUT"
log() { echo "==> $*"; }

case "$OS" in
freebsd)
    log "pkg install"
    env IGNORE_OSVERSION=yes ASSUME_ALWAYS_YES=yes pkg install -y rust cmake llvm gmake protobuf bash curl nasm pkgconf
    LIBCLANG_PATH="$(find /usr/local -name 'libclang.so*' 2>/dev/null | head -1 | xargs dirname)"; export LIBCLANG_PATH
    FEATURES=full-freebsd
    ;;
openbsd)
    P="https://cdn.openbsd.org/pub/OpenBSD/$(uname -r)/packages/$(uname -p)/"
    log "pkg_add ($P)"
    # curl はイメージに新しい版が入っていることがあり、パッケージと衝突する（入っていなければ入れる）
    env PKG_PATH="$P" pkg_add -I rust cmake gmake protobuf bash
    command -v curl >/dev/null || env PKG_PATH="$P" pkg_add -I curl
    if ! find /usr/local -name 'libclang*so*' 2>/dev/null | grep -q .; then
        LLVM="$(env PKG_PATH="$P" pkg_info -Q llvm 2>/dev/null | grep -E '^llvm-[0-9]' | sort -V | tail -1)"
        env PKG_PATH="$P" pkg_add -I "$LLVM"
    fi
    cat > /usr/local/bin/veil-cc <<'WRAP'
#!/bin/sh
for a in "$@"; do case "$a" in *.S|*.s) exec /usr/bin/cc "$@" ;; esac; done
exec /usr/bin/cc -include pthread.h "$@"
WRAP
    chmod +x /usr/local/bin/veil-cc
    LIBCXX="$(ls -1 /usr/lib/libc++.so.* | sort -V | tail -1)"; ln -sf "$LIBCXX" /usr/local/lib/libstdc++.so
    LIBCLANG_PATH="$(find /usr/local -name 'libclang.so*' 2>/dev/null | head -1 | xargs dirname)"; export LIBCLANG_PATH
    export CC_x86_64_unknown_openbsd=/usr/local/bin/veil-cc CC_aarch64_unknown_openbsd=/usr/local/bin/veil-cc
    export BINDGEN_EXTRA_CLANG_ARGS_x86_64_unknown_openbsd='-include pthread.h'
    export BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_openbsd='-include pthread.h'
    export CFLAGS_aarch64_unknown_openbsd='-DOPENSSL_STATIC_ARMCAP -DOPENSSL_STATIC_ARMCAP_NEON'
    export RUSTFLAGS='-L /usr/local/lib'
    FEATURES=full-openbsd
    ;;
netbsd)
    export PATH="/usr/pkg/bin:/usr/pkg/sbin:/usr/sbin:/sbin:$PATH"
    export PKG_PATH="https://cdn.NetBSD.org/pub/pkgsrc/packages/NetBSD/${ARCH}/$(uname -r | cut -d. -f1).0/All/"
    log "pkgin ($PKG_PATH)"
    [ -x /usr/pkg/bin/pkgin ] || /usr/sbin/pkg_add pkgin
    pkgin -y update
    pkgin -y install rust-bin cmake llvm clang protobuf gmake bash curl nasm pkgconf mozilla-rootcerts-openssl
    mozilla-rootcerts-openssl install >/dev/null 2>&1 || true
    LIBCLANG_PATH="$(find -L /usr/pkg -name 'libclang.so*' 2>/dev/null | head -1 | xargs dirname)"; export LIBCLANG_PATH
    [ "$ARCH" = aarch64 ] && export CFLAGS_aarch64_unknown_netbsd='-DOPENSSL_STATIC_ARMCAP -DOPENSSL_STATIC_ARMCAP_NEON'
    FEATURES=full-netbsd
    ;;
*) echo "unknown os: $OS" >&2; exit 2 ;;
esac

cd "$ROOT"
# fuzz はワークスペースのメンバーだが BSD では使わない（ビルドの対象外にする。bsd-vm.sh と同じ）
sed -i.bak -e 's|"fuzz", ||' Cargo.toml && rm -f Cargo.toml.bak
cargo --version
log "cargo build --profile dist --features $FEATURES"
cargo build --locked --profile dist --no-default-features --features "$FEATURES"
BIN="$ROOT/target/dist/veil"
# B-60: NetBSD は PaX MPROTECT がシステム全体で有効。wasmtime のために解除しておく（ELF の注記に残る）
[ "$OS" = netbsd ] && /usr/sbin/paxctl +m "$BIN"

if [ "${VEIL_BSD_UNIT_TESTS:-1}" = 1 ]; then
    log "cargo test --lib"
    if [ "$OS" = netbsd ]; then
        printf '#!/bin/sh\n/usr/sbin/paxctl +m "$1" >/dev/null 2>&1\nexec "$@"\n' > /tmp/paxrun.sh; chmod +x /tmp/paxrun.sh
        export CARGO_TARGET_$(echo "$TARGET" | tr 'a-z-' 'A-Z_')_RUNNER=/tmp/paxrun.sh
    fi
    # パイプで繋がない（sh に pipefail が無い OS があり、失敗が tail の終了コードに隠れる）
    if CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 \
        cargo test --locked --lib --no-default-features --features "$FEATURES" > "$OUT/unit-$TARGET.log" 2>&1; then
        grep '^test result' "$OUT/unit-$TARGET.log"
    else
        tail -60 "$OUT/unit-$TARGET.log"; exit 1
    fi
fi

log "archive"
VER="$(awk -F'"' '/^version = / { print $2; exit }' Cargo.toml)"
NAME="veil-${VER}-${TARGET}"
bash packaging/scripts/build-bsd.sh --os "$OS" --arch "$ARCH" --binary "$BIN" --os-version "$(uname -r)"
mv "packaging/output/$NAME.tar.gz" "$OUT/"
log "smoke test"
mkdir -p "$OUT/smoke" && tar -C "$OUT/smoke" -xzf "$OUT/$NAME.tar.gz"
if ! bash tools/release/binary-smoke.sh "$OUT/smoke/$NAME" "$VER" > "$OUT/smoke-$TARGET.log" 2>&1; then
    cat "$OUT/smoke-$TARGET.log"; exit 1
fi
cat "$OUT/smoke-$TARGET.log"
rm -rf "$OUT/smoke"
ls -l "$OUT"
