#!/usr/bin/env bash
# API ライブラリ（F-188〜F-195）の E2E。Rust・C・Python の各サンプルを実際にビルド・起動して叩く。
#
#   tests/api/library_e2e.sh [rust|c|python|nonregression|all]   （既定 all）
#
# 前提: cargo・gcc・curl・nc・python3。Python は PYTHON（既定: 作業用 venv を自動作成し maturin で導入）。
# 停止は pid 指定の kill（`pkill -f` は自分のシェルを巻き込むので使わない）。
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
WHAT="${1:-all}"
FAIL=0
PIDS=()
cleanup() { for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done; }
trap cleanup EXIT

ok()   { echo "  ok   $*"; }
bad()  { echo "  FAIL $*"; FAIL=1; }
free_port() { python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])'; }
wait_port() { for _ in $(seq 1 100); do (echo > /dev/tcp/127.0.0.1/$1) 2>/dev/null && return 0; sleep 0.1; done; return 1; }
expect() { # expect <name> <expected-substring> <command...>
  local name="$1" want="$2"; shift 2
  local got; got="$("$@" 2>&1)"
  if [[ "$got" == *"$want"* ]]; then ok "$name"; else bad "$name: want '$want' got '${got:0:300}'"; fi
}

run_rust() {
  echo "== Rust (examples/api/rust-showcase)"
  local T="$ROOT/target/examples-api"
  (cd "$ROOT/examples/api/rust-showcase" && CARGO_TARGET_DIR="$T" cargo build -q) || { bad "build"; return; }
  local W; W="$(mktemp -d)"; local P; P=$(free_port); local L4; L4=$(free_port); local U; U=$(free_port)
  cat > "$W/config.toml" <<TOML
[server]
listen = "127.0.0.1:$P"
[logging]
level = "info"
[[l4]]
name = "tcp_echo"
listen = "127.0.0.1:$L4"
handler = "tcp_echo"
[[l4]]
name = "udp_echo"
listen = "127.0.0.1:$U"
protocol = "udp"
handler = "udp_echo"
TOML
  (cd "$W" && exec "$T/debug/veil-example-showcase") > "$W/log" 2>&1 &
  local PID=$!; PIDS+=("$PID")
  wait_port "$P" || { bad "start"; cat "$W/log"; return; }
  local B="http://127.0.0.1:$P"
  expect "GET /hello" "Hello, World!" curl -s "$B/hello"
  expect "path+query params" '"email":"user7@example.com"' curl -s "$B/users/7?verbose=true"
  expect "422 path param" '"loc":["path","id"]' curl -s "$B/users/abc"
  expect "ApiError 404" "user 0 does not exist" curl -s "$B/users/0"
  expect "Query<T>" '{"users":["x1","x2"]}' curl -s "$B/users?limit=2&prefix=x"
  expect "Query<T> validation 422" '"less_than_equal"' curl -s "$B/users?limit=500"
  expect "POST 201" "201" curl -s -o /dev/null -w "%{http_code}" -X POST "$B/users" -d '{"name":"al","age":30}'
  expect "POST validation 422" '"too_short"' curl -s -X POST "$B/users" -d '{"name":"","age":200,"email":"bad"}'
  expect "offloaded sync fn" "500500" curl -s "$B/compute/1000"
  expect "catch-all" "file: a/b/c" curl -s "$B/files/a/b/c"
  expect "App::get" "pong" curl -s "$B/ping"
  expect "blocking handler" "done" curl -s "$B/blocking"
  expect "tower layer_on rejects" "401" curl -s -o /dev/null -w "%{http_code}" "$B/admin/stats"
  expect "tower layer_on accepts" '"created":' curl -s -H 'authorization: Bearer secret' "$B/admin/stats"
  expect "tower layer (global header)" "x-powered-by: veil" curl -s -i "$B/hello"
  expect "HEAD keeps length, no body" "content-length: 13" curl -s -I "$B/hello"
  expect "h2c prior knowledge" '"id":3' curl -s --http2-prior-knowledge "$B/users/3"
  expect "GraphQL POST" '{"data":{"add":3}}' curl -s -X POST "$B/graphql" -H 'content-type: application/json' -d '{"query":"{ add(a:1,b:2) }"}'
  expect "GraphQL GET" '{"data":{"add":5}}' curl -s "$B/graphql?query=%7Badd(a:2,b:3)%7D"
  expect "OpenAPI paths" '"/users/{id}"' curl -s "$B/openapi.json"
  expect "OpenAPI constraints" '"maxLength":50' curl -s "$B/openapi.json"
  expect "Swagger UI" "swagger-ui-bundle.js" curl -s "$B/docs"
  expect "404 for unknown" "404" curl -s -o /dev/null -w "%{http_code}" "$B/nope"
  expect "L4 TCP echo" "hello-tcp" bash -c "printf hello-tcp | timeout 3 nc -q1 127.0.0.1 $L4"
  expect "L4 UDP echo" "echo:hi" bash -c "printf hi | timeout 3 nc -u -w1 127.0.0.1 $U"
  expect "threads default = CPU count" "Workers: $(nproc)" cat "$W/log"
  # SIGHUP: ファイルを読み直しても組み込みのルート（マクロ）とコード側の上書きは保たれる
  sed -i 's/level = "info"/level = "warn"/' "$W/config.toml"; kill -HUP "$PID"; sleep 1
  expect "after SIGHUP routes kept" "Hello, World!" curl -s "$B/hello"
  kill "$PID"; for _ in $(seq 1 50); do kill -0 "$PID" 2>/dev/null || break; sleep 0.2; done
  kill -0 "$PID" 2>/dev/null && bad "graceful shutdown" || ok "graceful shutdown (SIGTERM)"
  # config.toml が無い場合はライブラリ既定（listen はコードの VEIL_LISTEN 上書きで変える）
  local W2; W2="$(mktemp -d)"; local P2; P2=$(free_port)
  (cd "$W2" && VEIL_LISTEN="127.0.0.1:$P2" exec "$T/debug/veil-example-showcase") > "$W2/log" 2>&1 &
  local PID2=$!; PIDS+=("$PID2")
  wait_port "$P2" && expect "no config.toml -> library defaults" "Hello, World!" curl -s "http://127.0.0.1:$P2/hello" || bad "no-config start"
  expect "defaults are plaintext h2c-only + HTTP/1.1" "H2C (HTTP/2 Cleartext) Server" cat "$W2/log"
  # 環境変数 VEIL_OVERRIDE_<n> もライブラリで効く
  kill "$PID2"; sleep 1
  local P3; P3=$(free_port)
  (cd "$W2" && VEIL_LISTEN="127.0.0.1:$P2" VEIL_OVERRIDE_1="api.openapi_enabled = false" exec "$T/debug/veil-example-showcase") > "$W2/log3" 2>&1 &
  PIDS+=("$!"); wait_port "$P2"
  expect "VEIL_OVERRIDE disables OpenAPI" "404" curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:$P2/openapi.json"
  kill "$!" 2>/dev/null
}

run_c() {
  echo "== C (examples/api/c-hello)"
  (cd "$ROOT" && cargo build -q -p veil-capi --release) || { bad "build libveil"; return; }
  make -s -C "$ROOT/examples/api/c-hello" hello hello-static >/dev/null || { bad "compile hello.c"; return; }
  for bin in hello hello-static; do
    local P; P=$(free_port)
    VEIL_LISTEN="127.0.0.1:$P" "$ROOT/examples/api/c-hello/$bin" > /tmp/veil-c-$$.log 2>&1 &
    local PID=$!; PIDS+=("$PID")
    wait_port "$P" || { bad "$bin start"; continue; }
    local B="http://127.0.0.1:$P"
    expect "$bin GET /" "Hello from C" curl -s "$B/"
    expect "$bin path param (decoded)" '{"id":"a b"}' curl -s "$B/users/a%20b"
    expect "$bin POST echo" "xyz" curl -s -X POST --data xyz "$B/echo"
    expect "$bin inline route h2c" "pong" curl -s --http2-prior-knowledge "$B/ping"
    expect "$bin OpenAPI from C metadata" '"summary":"Get a user"' curl -s "$B/openapi.json"
    kill "$PID"; sleep 0.5
  done
  rm -f /tmp/veil-c-$$.log
}

run_python() {
  echo "== Python (crates/veil-python)"
  local PY="${PYTHON:-}"
  if [ -z "$PY" ]; then
    local V="$ROOT/target/py-venv"
    [ -x "$V/bin/python" ] || python3 -m venv "$V"
    "$V/bin/pip" -q install maturin >/dev/null
    (cd "$ROOT/crates/veil-python" && VIRTUAL_ENV="$V" PATH="$V/bin:$PATH" maturin develop -q --release) >/dev/null 2>&1 || { bad "maturin develop"; return; }
    PY="$V/bin/python"
  fi
  (cd "$ROOT/crates/veil-python" && "$PY" -I tests/test_binding.py) >/dev/null 2>&1 && ok "unit tests" || bad "unit tests"
  (cd "$ROOT/crates/veil-python" && PYTHON="$PY" bash tests/e2e.sh) > /tmp/veil-py-$$.log 2>&1 && ok "e2e.sh (both examples)" || { bad "e2e.sh"; grep FAIL /tmp/veil-py-$$.log; }
  rm -f /tmp/veil-py-$$.log
}

run_nonregression() {
  echo "== 本体ビルドにライブラリのコードが入っていない"
  (cd "$ROOT" && cargo build -q --features full) || { bad "build full"; return; }
  local n; n=$(nm -C "$ROOT/target/debug/veil" 2>/dev/null | grep -c "veil::api::") || true
  [ "$n" = 0 ] && ok "no veil::api symbols in the full binary" || bad "veil::api symbols found ($n)"
  n=$(nm -C "$ROOT/target/debug/veil" 2>/dev/null | grep -ciE "inventory|async_graphql|tokio::runtime|pyo3") || true
  [ "$n" = 0 ] && ok "no inventory/async-graphql/tokio/pyo3 in the full binary" || bad "library dependencies linked ($n)"
}

case "$WHAT" in
  rust) run_rust ;;
  c) run_c ;;
  python) run_python ;;
  nonregression) run_nonregression ;;
  all) run_nonregression; run_rust; run_c; run_python ;;
  *) echo "usage: $0 [rust|c|python|nonregression|all]"; exit 2 ;;
esac
[ "$FAIL" = 0 ] && echo "ALL OK" || { echo "SOME CHECKS FAILED"; exit 1; }
