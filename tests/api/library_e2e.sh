#!/usr/bin/env bash
# API ライブラリ（F-188〜F-195）の E2E。Rust・C・Python の各サンプルを実際にビルド・起動して叩く。
#
#   tests/api/library_e2e.sh [rust|grpc|c|python|nonregression|all]   （既定 all）
#
# 前提: cargo・cc・make・curl（HTTP/2 対応）・python3。Python は PYTHON（既定: 作業用 venv を自動作成し maturin で導入）。
# Linux・macOS・FreeBSD・OpenBSD・NetBSD・Windows（Git Bash）で動く。python3 の名前が違う環境は PY3 で指定する
# （Windows の python3 は Microsoft Store のスタブのことがある）。本体の feature は OS ごとの `full*` を自動で選ぶ。
# 停止は pid 指定の kill（`pkill -f` は自分のシェルを巻き込むので使わない）。
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
WHAT="${1:-all}"
FAIL=0
PIDS=()
trap 'cleanup' EXIT

OS="$(uname -s)"
case "$OS" in
  Linux) FULL=full; NODEF="" ;;
  Darwin) FULL=full-macos; NODEF=--no-default-features ;;
  FreeBSD) FULL=full-freebsd; NODEF=--no-default-features ;;
  OpenBSD) FULL=full-openbsd; NODEF=--no-default-features ;;
  NetBSD) FULL=full-netbsd; NODEF=--no-default-features ;;
  MINGW*|MSYS*|CYGWIN*) OS=Windows; FULL=full-windows; NODEF=--no-default-features ;;
  *) FULL=full; NODEF="" ;;
esac
PY3="${PY3:-python3}"
EXE=""; [ "$OS" = Windows ] && EXE=.exe
TMPD="$(mktemp -d)"
cleanup() { for p in ${PIDS[@]+"${PIDS[@]}"}; do kill "$p" 2>/dev/null || true; done; rm -rf "$TMPD"; }
# timeout(1) は macOS・OpenBSD の基本システムに無い
tmo() { if command -v timeout >/dev/null; then timeout "$@"; elif command -v gtimeout >/dev/null; then gtimeout "$@";
        else perl -e '$t = shift; $SIG{ALRM} = sub { kill 9, $p; exit 124 }; $p = fork // die; if (!$p) { exec @ARGV } alarm $t; waitpid $p, 0; exit $? >> 8' "$@"; fi; }
ncpu() { nproc 2>/dev/null || sysctl -n hw.ncpu; }
# L4 のエコー（nc のオプションは実装ごとに違うので Python で送受信する）
l4_echo() { # l4_echo tcp|udp <port> <payload>
  "$PY3" -I -c '
import socket, sys
kind, port, data = sys.argv[1], int(sys.argv[2]), sys.argv[3].encode()
s = socket.socket(socket.AF_INET, socket.SOCK_STREAM if kind == "tcp" else socket.SOCK_DGRAM)
s.settimeout(3)
if kind == "tcp":
    s.connect(("127.0.0.1", port)); s.sendall(data); s.shutdown(socket.SHUT_WR)
    out = b""
    while True:
        b = s.recv(65536)
        if not b: break
        out += b
else:
    s.sendto(data, ("127.0.0.1", port)); out = s.recv(65536)
print(out.decode())' "$@"; }

ok()   { echo "  ok   $*"; }
bad()  { echo "  FAIL $*"; FAIL=1; }
free_port() { "$PY3" -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])'; }
# wait_port <port> [pid]: 最大 30 秒（VM を同時に動かす高負荷のホストでは起動に 10 秒を超えることがある）。
# pid を渡すと、そのプロセスが終了した時点で失敗にする。
wait_port() {
  for _ in $(seq 1 300); do
    (echo > /dev/tcp/127.0.0.1/$1) 2>/dev/null && return 0
    [ -n "${2:-}" ] && ! kill -0 "$2" 2>/dev/null && return 1
    sleep 0.1
  done
  return 1
}
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
listen_cleartext = "127.0.0.1:$P"
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
  (cd "$W" && exec "$T/debug/veil-example-showcase$EXE") > "$W/log" 2>&1 &
  local PID=$!; PIDS+=("$PID")
  wait_port "$P" "$PID" || { bad "start"; cat "$W/log"; return; }
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
  expect "stream (HTTP/1.1 chunked)" "line 3" curl -s "$B/stream"
  expect "stream trailer (HTTP/1.1)" "x-lines: 3" curl -s -i "$B/stream"
  expect "stream trailer (h2c)" "x-lines: 3" curl -s -i --http2-prior-knowledge "$B/stream"
  expect "stream HEAD (no body)" "transfer-encoding: chunked" bash -c "curl -s -I '$B/stream' | tr A-Z a-z"
  expect "streaming request (HTTP/1.1)" "HELLO STREAM" curl -s -X POST "$B/upper" -d 'hello stream'
  expect "streaming request (h2c)" "HELLO H2" curl -s --http2-prior-knowledge -X POST "$B/upper" -d 'hello h2'
  expect "streaming request (h2c, 1MiB)" "1048576" bash -c "dd if=/dev/zero bs=1024 count=1024 2>/dev/null | tr '\\0' a | curl -s --http2-prior-knowledge -X POST --data-binary @- '$B/upper' | wc -c"
  expect "OpenAPI paths" '"/users/{id}"' curl -s "$B/openapi.json"
  expect "OpenAPI constraints" '"maxLength":50' curl -s "$B/openapi.json"
  expect "Swagger UI" "swagger-ui-bundle.js" curl -s "$B/docs"
  expect "404 for unknown" "404" curl -s -o /dev/null -w "%{http_code}" "$B/nope"
  expect "L4 TCP echo" "hello-tcp" l4_echo tcp "$L4" hello-tcp
  expect "L4 UDP echo" "echo:hi" l4_echo udp "$U" hi
  expect "threads default = CPU count" "Workers: $(ncpu)" cat "$W/log"
  # SIGHUP: ファイルを読み直しても組み込みのルート（マクロ）とコード側の上書きは保たれる
  if [ "$OS" != Windows ]; then  # Windows に SIGHUP は無い
    sed -i.bak 's/level = "info"/level = "warn"/' "$W/config.toml"; kill -HUP "$PID"; sleep 1
    expect "after SIGHUP routes kept" "Hello, World!" curl -s "$B/hello"
  fi
  kill "$PID"; for _ in $(seq 1 50); do kill -0 "$PID" 2>/dev/null || break; sleep 0.2; done
  kill -0 "$PID" 2>/dev/null && bad "graceful shutdown" || ok "graceful shutdown (SIGTERM)"
  # config.toml が無い場合はライブラリ既定（listen はコードの VEIL_LISTEN 上書きで変える）
  local W2; W2="$(mktemp -d)"; local P2; P2=$(free_port)
  (cd "$W2" && VEIL_LISTEN="127.0.0.1:$P2" exec "$T/debug/veil-example-showcase$EXE") > "$W2/log" 2>&1 &
  local PID2=$!; PIDS+=("$PID2")
  wait_port "$P2" && expect "no config.toml -> library defaults" "Hello, World!" curl -s "http://127.0.0.1:$P2/hello" || bad "no-config start"
  expect "defaults are plaintext h2c-only + HTTP/1.1" "H2C (HTTP/2 Cleartext) Server" cat "$W2/log"
  # 環境変数 VEIL_OVERRIDE_<n> もライブラリで効く
  kill "$PID2"; sleep 1
  local P3; P3=$(free_port)
  (cd "$W2" && VEIL_LISTEN="127.0.0.1:$P2" VEIL_OVERRIDE_1="api.openapi_enabled = false" exec "$T/debug/veil-example-showcase$EXE") > "$W2/log3" 2>&1 &
  PIDS+=("$!"); wait_port "$P2"
  expect "VEIL_OVERRIDE disables OpenAPI" "404" curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:$P2/openapi.json"
  kill "$!" 2>/dev/null
}

run_graphql() {
  echo "== GraphQL (examples/api/rust-graphql、async-graphql を利用者側で載せる。F-207)"
  local T="$ROOT/target/examples-api"
  (cd "$ROOT/examples/api/rust-graphql" && CARGO_TARGET_DIR="$T" cargo build -q) || { bad "build graphql"; return; }
  local W; W="$(mktemp -d)"; local P; P=$(free_port)
  (cd "$W" && VEIL_LISTEN="127.0.0.1:$P" exec "$T/debug/veil-example-graphql$EXE") > "$W/log" 2>&1 &
  local PID=$!; PIDS+=("$PID")
  wait_port "$P" "$PID" || { bad "start graphql"; cat "$W/log"; return; }
  local B="http://127.0.0.1:$P/graphql" J='content-type: application/json'
  expect "GraphQL POST" '{"data":{"add":3}}' curl -s -X POST "$B" -H "$J" -d '{"query":"{ add(a:1,b:2) }"}'
  expect "GraphQL GET" '{"data":{"add":5}}' curl -s "$B?query=%7Badd(a:2,b:3)%7D"
  expect "GraphQL variables" '{"data":{"greeting":"Hello, veil!"}}' curl -s -X POST "$B" -H "$J" -d '{"query":"query($n:String!){ greeting(name:$n) }","variables":{"n":"veil"}}'
  expect "GraphQL mutation" '{"data":{"increment":5}}' curl -s -X POST "$B" -H "$J" -d '{"query":"mutation { increment(by:5) }"}'
  expect "GraphQL batch" '[{"data":{"add":2}},{"data":{"count":5}}]' curl -s -X POST "$B" -H "$J" -d '[{"query":"{ add(a:1,b:1) }"},{"query":"{ count }"}]'
  expect "GraphQL error" '"errors"' curl -s -X POST "$B" -H "$J" -d '{"query":"{ nope }"}'
  expect "GraphQL response media type" "application/graphql-response+json" curl -s -i -X POST "$B" -H "$J" -H 'accept: application/graphql-response+json' -d '{"query":"{ add(a:1,b:1) }"}'
  expect "GraphiQL" "graphiql" bash -c "curl -s -H 'accept: text/html' '$B' | tr A-Z a-z"
  expect "subscription over SSE" 'event: next
data: {"data":{"countdown":0}}

event: complete' curl -s -N -X POST "$B" -H "$J" -H 'accept: text/event-stream' -d '{"query":"subscription { countdown(from: 2) }"}'
  expect "subscription over SSE (h2c)" '"countdown":2' curl -s -N --http2-prior-knowledge -X POST "$B" -H "$J" -H 'accept: text/event-stream' -d '{"query":"subscription { countdown(from: 2) }"}'
  kill "$PID" 2>/dev/null
}

run_c() {
  echo "== C (examples/api/c-hello)"
  (cd "$ROOT" && cargo build -q -p veil-capi --release) || { bad "build libveil"; return; }
  local CH="$ROOT/examples/api/c-hello"
  # 前回（別の OS・アーキからコピーされた作業ツリーを含む）の実行ファイルを make が新しいと見なさないように
  rm -f "$CH/hello" "$CH/hello-static" "$CH/hello.exe" "$CH/hello-static.exe"
  if [ "$OS" = Linux ]; then
    make -s -C "$CH" hello hello-static >/dev/null || { bad "compile hello.c"; return; }
  else
    # 静的リンクに要るシステムライブラリは OS ごとに違う（OpenBSD・NetBSD に libdl は無い等）ので rustc に聞く
    local libs; libs=$(cd "$ROOT" && cargo rustc -q -p veil-capi --release --crate-type staticlib -- \
      --print native-static-libs 2>&1 | sed -n 's/.*native-static-libs: //p' | tail -1)
    if [ "$OS" = Windows ]; then
      c_msvc "$CH" "$libs" || { bad "compile hello.c (MSVC)"; return; }
    else
      # Makefile は GNU make の関数（abspath）を使う
      local MK; MK=$(command -v gmake || command -v make)
      "$MK" -s -C "$CH" CC=cc LIBS="$libs" hello hello-static >/dev/null || { bad "compile hello.c"; return; }
    fi
  fi
  for bin in hello hello-static; do
    local P; P=$(free_port)
    VEIL_LISTEN="127.0.0.1:$P" PATH="$ROOT/target/release:$PATH" "$CH/$bin$EXE" > "$TMPD/c.log" 2>&1 &
    local PID=$!; PIDS+=("$PID")
    wait_port "$P" "$PID" || { bad "$bin start"; cat "$TMPD/c.log"; continue; }
    local B="http://127.0.0.1:$P"
    expect "$bin GET /" "Hello from C" curl -s "$B/"
    expect "$bin path param (decoded)" '{"id":"a b"}' curl -s "$B/users/a%20b"
    expect "$bin POST echo" "xyz" curl -s -X POST --data xyz "$B/echo"
    expect "$bin inline route h2c" "pong" curl -s --http2-prior-knowledge "$B/ping"
    expect "$bin OpenAPI from C metadata" '"summary":"Get a user"' curl -s "$B/openapi.json"
    kill "$PID"; sleep 0.5
  done
}

# Windows: MSVC（vcvars64.bat。VCVARS で指定、既定は vswhere で探す）で hello.c をビルドする
c_msvc() { # c_msvc <dir> <native-static-libs>
  local dir="$1" libs="$2" vc="${VCVARS:-}"
  if [ -z "$vc" ]; then
    local vs; vs=$("/c/Program Files (x86)/Microsoft Visual Studio/Installer/vswhere.exe" -latest -property installationPath | tr -d '\r')
    vc="$vs\\VC\\Auxiliary\\Build\\vcvars64.bat"
  fi
  local rel; rel="$(cygpath -w "$ROOT/target/release")"
  local inc; inc="$(cygpath -w "$ROOT/crates/veil-capi/include")"
  # native-static-libs の windows.0.NN.0.lib は windows-targets 系クレート（windows_x86_64_msvc）が同梱する
  # インポートライブラリ。cargo のレジストリにあるので、その場所をリンカの検索パスに足す
  local extra="" l d
  for l in $libs; do
    case "$l" in
      windows.*.lib)
        d=$(find "$HOME/.cargo/registry/src" -name "$l" -path '*x86_64_msvc*' 2>/dev/null | head -1)
        [ -n "$d" ] && extra="$extra /LIBPATH:\"$(cygpath -w "$(dirname "$d")")\"" ;;
    esac
  done
  # /utf-8: ソースのコメント（UTF-8 の日本語）をコードページで読まない
  cat > "$dir/build-msvc.bat" <<BAT
@call "$vc" >nul || exit /b 1
cl /nologo /W3 /utf-8 /I"$inc" hello.c /Fe:hello.exe /link /LIBPATH:"$rel" veil.dll.lib || exit /b 1
cl /nologo /W3 /utf-8 /I"$inc" hello.c /Fe:hello-static.exe /link "$rel\\veil.lib" $extra $libs || exit /b 1
BAT
  (cd "$dir" && cmd //c build-msvc.bat) > "$TMPD/msvc.log" 2>&1 || { tail -20 "$TMPD/msvc.log"; rm -f "$dir/build-msvc.bat"; return 1; }
  rm -f "$dir/build-msvc.bat" "$dir"/*.obj
}

run_python() {
  echo "== Python (crates/veil-python)"
  local PY="${PYTHON:-}"
  if [ -z "$PY" ]; then
    local V="$ROOT/target/py-venv"
    local VB="$V/bin"; [ "$OS" = Windows ] && VB="$V/Scripts"
    [ -x "$VB/python$EXE" ] || "$PY3" -m venv "$V"
    "$VB/python$EXE" -m pip -q install maturin >/dev/null
    (cd "$ROOT/crates/veil-python" && unset CONDA_PREFIX && VIRTUAL_ENV="$V" PATH="$VB:$PATH" maturin develop -q --release) > "$TMPD/maturin.log" 2>&1 || { bad "maturin develop"; tail -20 "$TMPD/maturin.log"; return; }
    PY="$VB/python$EXE"
  fi
  (cd "$ROOT/crates/veil-python" && "$PY" -I tests/test_binding.py) >/dev/null 2>&1 && ok "unit tests" || bad "unit tests"
  (cd "$ROOT/crates/veil-python" && PYTHON="$PY" bash tests/e2e.sh) > "$TMPD/py.log" 2>&1 && ok "e2e.sh (both examples)" || { bad "e2e.sh"; grep -E "FAIL|server" "$TMPD/py.log"; }
}

run_grpc() {
  echo "== gRPC (examples/api/rust-grpc + tonic / quinn+h3 クライアント、F-197)"
  local T="$ROOT/target/examples-api"
  (cd "$ROOT/tests/api/grpc_client" && CARGO_TARGET_DIR="$T" cargo build -q) || { bad "build client"; return; }
  grpc_interop "$T" ""
  # Linux は既定の io_uring に加えて epoll の reactor でも確かめる（B-123 は reactor でだけ固まった）
  [ "$OS" = Linux ] && grpc_interop "$ROOT/target/examples-api-epoll" "veil/epoll"
}

grpc_interop() { # grpc_interop <target-dir> <extra features>
  local T="$1" F="$2" label="${2:-default}"
  (cd "$ROOT/examples/api/rust-grpc" && CARGO_TARGET_DIR="$T" cargo build -q ${F:+--features "$F"}) || { bad "build server ($label)"; return; }
  local W; W="$(mktemp -d)"; local P; P=$(free_port); local S; S=$(free_port)
  # 下の openssl は引数のパス変換を止めるので、作業ディレクトリは Windows の形（C:/...）で渡す
  [ "$OS" = Windows ] && W="$(cygpath -m "$W")"
  # MSYS_NO_PATHCONV（Git for Windows）・MSYS2_ARG_CONV_EXCL（MSYS2）: Git Bash が `/CN=...` を Windows のパスに書き換えないように
  MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*' openssl req -x509 -newkey rsa:2048 -nodes -keyout "$W/key.pem" -out "$W/cert.pem" -days 2 \
    -subj /CN=localhost -addext subjectAltName=DNS:localhost,IP:127.0.0.1 2>/dev/null || { bad "cert"; return; }
  (cd "$W" && VEIL_LISTEN="127.0.0.1:$P" VEIL_TLS_LISTEN="127.0.0.1:$S" VEIL_TLS_CERT="$W/cert.pem" \
    VEIL_TLS_KEY="$W/key.pem" exec "$T/debug/veil-example-grpc$EXE") > "$W/log" 2>&1 &
  local PID=$!; PIDS+=("$PID")
  wait_port "$P" "$PID" || { bad "start ($label)"; cat "$W/log"; return; }
  local out; out="$(tmo 300 "$ROOT/target/examples-api/debug/veil-grpc-check$EXE" --h2c "127.0.0.1:$P" --h3 "127.0.0.1:$S" 2>&1)"
  local rc=$?
  echo "$out" | sed 's/^/  /'
  [ "$rc" = 0 ] && ok "grpc interop ($label, exit 0)" || bad "grpc interop ($label, exit $rc)"
  kill "$PID"
}

run_nonregression() {
  echo "== 本体ビルドにライブラリのコードが入っていない（${FULL}）"
  (cd "$ROOT" && cargo build -q $NODEF --features "$FULL") || { bad "build $FULL"; return; }
  # シンボル一覧。Windows（MSVC）は実行ファイルに名前が無いので PDB の文字列を見る
  local SYMS="$TMPD/syms"
  if [ "$OS" = Windows ]; then grep -a -o '[[:print:]]\{6,\}' "$ROOT/target/debug/veil.pdb" > "$SYMS"
  else nm -C "$ROOT/target/debug/veil" > "$SYMS" 2>/dev/null; fi
  [ -s "$SYMS" ] || { bad "no symbols to inspect"; return; }
  local n; n=$(grep -c "veil::api::" "$SYMS") || true
  [ "$n" = 0 ] && ok "no veil::api symbols in the full binary" || bad "veil::api symbols found ($n)"
  n=$(grep -ciE "inventory|async_graphql|tokio::runtime|pyo3" "$SYMS") || true
  [ "$n" = 0 ] && ok "no inventory/async-graphql/tokio/pyo3 in the full binary" || bad "library dependencies linked ($n)"
  n=$(grep -cE "veil_build::|prost_build::|(^|[^a-z_])tonic::" "$SYMS") || true
  [ "$n" = 0 ] && ok "no veil-build/prost-build/tonic in the full binary" || bad "gRPC library code linked ($n)"
  n=$(grep -cE "veil::tokio::" "$SYMS") || true
  [ "$n" = 0 ] && ok "no veil::tokio (tokio-compat) in the full binary" || bad "veil::tokio symbols found ($n)"
  # F-198: api の既定（tokio-compat）は tokio に依存しない
  local t; t=$(cd "$ROOT" && cargo tree -q -e normal,build --no-default-features --features "$FULL",api -i tokio 2>&1 | head -1) || true
  case "$t" in
    *"did not match"*|"") ok "no tokio in the dependency tree of features=api" ;;
    *) bad "tokio is a dependency of features=api: $t" ;;
  esac
  # F-207: GraphQL は利用者が選ぶ（api に async-graphql を含めない）。F-208: gRPC ハンドラは prost に依存しない
  # （veil で prost を使うのは VDS だけ。vds を含まない構成で確かめる）
  t=$(cd "$ROOT" && cargo tree -q -e normal --no-default-features --features api -i async-graphql 2>&1 | head -1) || true
  case "$t" in
    *"did not match"*|"") ok "no async-graphql in the dependency tree of features=api" ;;
    *) bad "async-graphql is a dependency of features=api: $t" ;;
  esac
  t=$(cd "$ROOT" && cargo tree -q -e normal --no-default-features --features "http2,grpc-handler" -i prost 2>&1 | head -1) || true
  case "$t" in
    *"did not match"*|"") ok "no prost in the dependency tree of features=grpc-handler" ;;
    *) bad "prost is a dependency of features=grpc-handler: $t" ;;
  esac
}

case "$WHAT" in
  rust) run_rust; run_graphql ;;
  grpc) run_grpc ;;
  c) run_c ;;
  python) run_python ;;
  nonregression) run_nonregression ;;
  all) run_nonregression; run_rust; run_graphql; run_grpc; run_c; run_python ;;
  *) echo "usage: $0 [rust|grpc|c|python|nonregression|all]"; exit 2 ;;
esac
[ "$FAIL" = 0 ] && echo "ALL OK" || { echo "SOME CHECKS FAILED"; exit 1; }
