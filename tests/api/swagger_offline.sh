#!/usr/bin/env bash
# F-264: /docs（同梱の Swagger UI）が外へ一切通信せずに描画されることを、ネットワークの無いコンテナ
# （`docker run --network none`、ループバックだけ）の中のヘッドレスの Chromium（Playwright）で確かめる。
#
#   tests/api/swagger_offline.sh [rust|python|all]   （既定 all。Linux・docker が要る）
#
# Rust は examples/api/rust-hello、Python は examples/api/python-hello（このリポジトリの wheel を入れる）。
# どちらもホストでビルドしたものをコンテナへ読み取り専用で渡す（コンテナの中ではビルドしない）。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
WHAT="${1:-all}"
IMG="${PLAYWRIGHT_IMAGE:-mcr.microsoft.com/playwright/python:v1.55.0-noble}"
T="$ROOT/target/examples-api"
W="$ROOT/target/swagger-offline"
mkdir -p "$W"
FAIL=0

run_in_box() { # run_in_box <server command (in the container)> <expected paths...>
  local cmd="$1"; shift
  local args; args=$(printf '%q ' "$@")
  docker run --rm --network none --user "$(id -u):$(id -g)" -e HOME=/tmp \
    -v "$ROOT:/repo:ro" -v "$W:/w:ro" -w /tmp "$IMG" bash -c "
      set -e
      $cmd > /tmp/server.log 2>&1 &
      for _ in \$(seq 1 200); do (echo > /dev/tcp/127.0.0.1/8080) 2>/dev/null && break; sleep 0.1; done
      python3 /repo/tests/api/swagger_offline.py http://127.0.0.1:8080 $args || { cat /tmp/server.log; exit 1; }
    " || FAIL=1
}

if [ "$WHAT" = rust ] || [ "$WHAT" = all ]; then
  echo "== Rust (examples/api/rust-hello)"
  (cd "$ROOT/examples/api/rust-hello" && CARGO_TARGET_DIR="$T" cargo build -q)
  cp "$T/debug/veil-example-hello" "$W/"
  run_in_box "/w/veil-example-hello" "/hello/{name}"
fi

if [ "$WHAT" = python ] || [ "$WHAT" = all ]; then
  echo "== Python (examples/api/python-hello)"
  rm -rf "$W/wheels"
  python3 -m pip -q install --user "maturin>=1.5,<2.0" >/dev/null 2>&1 || true
  (cd "$ROOT/crates/veil-python" && python3 -m maturin build -q --release -o "$W/wheels")
  # pip は wheel をネットワーク無しで入れる（依存は無い）
  run_in_box "python3 -m pip install -q --no-index --no-deps --target /tmp/py /w/wheels/*.whl && \
    PYTHONPATH=/tmp/py VEIL_LISTEN=127.0.0.1:8080 python3 /repo/examples/api/python-hello/app.py" \
    "/ping" "/hello/{name}" "/users"
fi

[ "$FAIL" = 0 ] && echo "ALL OK" || { echo "SOME CHECKS FAILED"; exit 1; }
