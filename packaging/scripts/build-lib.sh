#!/usr/bin/env bash
# veil のライブラリ成果物（Rust ソース・C ライブラリ・Python wheel）をビルドして 1 個の zip にまとめる（F-195）。
#
#   packaging/scripts/build-lib.sh [--kind all|rust|c|python[,..]] [--docker|--local] [--profile dist|release]
#
#   --docker（既定）: docker/Dockerfile.lib（manylinux_2_28）でビルドする。glibc 2.28 以上で動く成果物になる。
#   --local         : このホストの cargo・maturin でビルドする（ホストの glibc 以上でしか動かない）。
#
# 出力: packaging/output/lib/veil-lib-<version>-<arch>-linux-gnu.zip
# （veil 本体のパッケージ packaging/output/ の .deb・.rpm・.tar.gz とは別ディレクトリ・別名）
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
KINDS="rust,c,python"
MODE=docker
PROFILE=dist
while [ $# -gt 0 ]; do
  case "$1" in
    --kind) KINDS="$2"; shift 2 ;;
    --docker) MODE=docker; shift ;;
    --local) MODE=local; shift ;;
    --profile) PROFILE="$2"; shift 2 ;;
    -h|--help) sed -n 2,12p "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done
[ "$KINDS" = all ] && KINDS="rust,c,python"
OUT="$ROOT/packaging/output/lib"
mkdir -p "$OUT"

if [ "$MODE" = docker ]; then
  # type=local は成果物の所有者を root にしようとして非 root の docker（snap 等）で失敗するので、
  # tar で受け取って展開する（展開は実行ユーザーの所有になる）。
  docker build -f "$ROOT/docker/Dockerfile.lib" --target export \
    --build-arg LIB_KINDS="$KINDS" --build-arg CARGO_PROFILE="$PROFILE" \
    --output "type=tar,dest=$OUT/.export.tar" "$ROOT"
  tar -xf "$OUT/.export.tar" -C "$OUT" --no-same-owner
  rm -f "$OUT/.export.tar"
  ls -la "$OUT"/veil-lib-*.zip
  exit 0
fi

cd "$ROOT"
WHEELS="$ROOT/target/lib-wheels"
rm -rf "$WHEELS"; mkdir -p "$WHEELS"
if [[ ",$KINDS," == *",c,"* ]]; then
  cargo build --locked --profile "$PROFILE" -p veil-capi
fi
if [[ ",$KINDS," == *",python,"* ]]; then
  command -v maturin >/dev/null || { echo "maturin is required (pip install maturin)" >&2; exit 1; }
  (cd crates/veil-python && maturin build --locked --profile "$PROFILE" --out "$WHEELS")
fi
bash "$ROOT/packaging/scripts/assemble-lib-zip.sh" "$ROOT" "$OUT" "$KINDS" "$ROOT/target" "$PROFILE" "$WHEELS"
