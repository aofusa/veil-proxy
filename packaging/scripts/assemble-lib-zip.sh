#!/usr/bin/env bash
# ライブラリ成果物を 1 個の zip にまとめる（F-195）。build-lib.sh / docker/Dockerfile.lib から呼ばれる。
#
#   assemble-lib-zip.sh <repo-root> <out-dir> <kinds(csv: rust,c,python)> <cargo-target-dir> <cargo-profile-dir> [wheel-dir]
#
# zip の中身（既存の veil バイナリの成果物と混ざらないよう、名前は veil-lib-* で始める）:
#   veil-lib-<ver>-<arch>-linux-gnu/
#     README.md  LICENSE-APACHE  LICENSE-MIT  MANIFEST.txt
#     c/include/veil.h  c/lib/libveil.so  c/lib/libveil.a  c/lib/pkgconfig/veil.pc  c/examples/...
#     python/veil_api-<ver>-cp38-abi3-<platform>.whl  python/examples/...
#     rust/veil-src-<ver>.tar.gz  rust/examples/...
#     docs/library.md  docs/ja/library.md
set -euo pipefail
ROOT="$1"; OUT="$2"; KINDS="$3"; TGT="$4"; PROF="$5"; WHEELS="${6:-}"
VER=$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)
ARCH=$(uname -m)
NAME="veil-lib-${VER}-${ARCH}-linux-gnu"
STAGE="$(mktemp -d)/$NAME"
mkdir -p "$STAGE/docs/ja" "$OUT"
has() { [[ ",$KINDS," == *",$1,"* ]]; }

cp "$ROOT/LICENSE-APACHE" "$ROOT/LICENSE-MIT" "$STAGE/"
cp "$ROOT/docs/guide/library.md" "$STAGE/docs/library.md"
cp "$ROOT/docs/guide/ja/library.md" "$STAGE/docs/ja/library.md"

if has c; then
  mkdir -p "$STAGE/c/include" "$STAGE/c/lib/pkgconfig" "$STAGE/c/examples"
  cp "$ROOT/crates/veil-capi/include/veil.h" "$STAGE/c/include/"
  cp "$TGT/$PROF/libveil.so" "$TGT/$PROF/libveil.a" "$STAGE/c/lib/"
  cp "$ROOT/crates/veil-capi/README.md" "$STAGE/c/README.md"
  cp -r "$ROOT/examples/api/c-hello" "$STAGE/c/examples/"
  rm -f "$STAGE/c/examples/c-hello/hello" "$STAGE/c/examples/c-hello/hello-static"
  cat > "$STAGE/c/lib/pkgconfig/veil.pc" <<PC
prefix=\${pcfiledir}/../..
libdir=\${prefix}/lib
includedir=\${prefix}/include

Name: veil
Description: veil API server library (C API)
Version: $VER
Libs: -L\${libdir} -lveil
Libs.private: -lpthread -ldl -lm
Cflags: -I\${includedir}
PC
fi

if has python; then
  mkdir -p "$STAGE/python/examples"
  ls "$WHEELS"/*.whl >/dev/null 2>&1 || { echo "no wheel in $WHEELS" >&2; exit 1; }
  cp "$WHEELS"/*.whl "$STAGE/python/"
  cp "$ROOT/crates/veil-python/README.md" "$STAGE/python/README.md"
  cp -r "$ROOT/examples/api/python-hello" "$ROOT/examples/api/python-fastapi-like" "$STAGE/python/examples/"
fi

if has rust; then
  mkdir -p "$STAGE/rust/examples"
  # 本体 + crates/ + third_party（vendoring 版 quiche・wasmtime）+ fuzz のマニフェスト（ワークスペース解決用）
  ( cd "$ROOT" && tar czf "$STAGE/rust/veil-src-$VER.tar.gz" \
      --exclude='target' --exclude='__pycache__' --exclude='*.so' \
      --transform "s,^,veil-$VER/," \
      Cargo.toml Cargo.lock build.rs clippy.toml .cargo src crates third_party benches \
      fuzz/Cargo.toml fuzz/fuzz_targets LICENSE-APACHE LICENSE-MIT README.md )
  cp -r "$ROOT/examples/api/rust-hello" "$ROOT/examples/api/rust-showcase" "$STAGE/rust/examples/"
  rm -rf "$STAGE"/rust/examples/*/target
  cat > "$STAGE/rust/README.md" <<MD
# veil (Rust library) $VER

Extract \`veil-src-$VER.tar.gz\` and depend on it by path (or use the git repository):

\`\`\`toml
[dependencies]
veil = { path = "veil-$VER", default-features = false, features = ["full", "api"] }

# Recommended for HTTP/3 performance (veil's vendored quiche, F-172)
[patch.crates-io]
quiche = { path = "veil-$VER/third_party/quiche" }
\`\`\`

See \`examples/rust-showcase\` and \`../docs/library.md\`.
MD
fi

cat > "$STAGE/README.md" <<MD
# veil API server library $VER ($ARCH, Linux glibc)

| Directory | Contents |
|-----------|----------|
$(has c && echo "| \`c/\` | C API: \`include/veil.h\`, \`lib/libveil.so\`, \`lib/libveil.a\`, \`lib/pkgconfig/veil.pc\`, example |")
$(has python && echo "| \`python/\` | Python wheel (\`pip install python/*.whl\`, import \`veil\`), examples |")
$(has rust && echo "| \`rust/\` | Rust source package and example (depend on it by path) |")
| \`docs/\` | Library guide (English / Japanese) |

This archive contains only the library artifacts; the \`veil\` proxy binaries are released separately.
MD
( cd "$STAGE" && find . -type f | sort > MANIFEST.txt )
rm -f "$OUT/$NAME.zip"
( cd "$(dirname "$STAGE")" && zip -qr "$OUT/$NAME.zip" "$NAME" )
echo "$OUT/$NAME.zip"
