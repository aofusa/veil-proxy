#!/usr/bin/env bash
# Python バインディングの E2E: 各サンプルを空きポートで起動し、curl（HTTP/1.1 と h2c prior knowledge）で検証する。
# 前提: `maturin develop --release` 済みの venv を有効化（または PYTHON=/path/to/venv/bin/python）。
#   PYTHON=/tmp/claude-1000/pyvenv/bin/python crates/veil-python/tests/e2e.sh
set -u
PYTHON="${PYTHON:-python3}"
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
FAIL=0
PID=""
LOG="$(mktemp)"

ok()   { echo "  ok   $*"; }
bad()  { echo "  FAIL $*"; FAIL=$((FAIL + 1)); }
WINDOWS=""; case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) WINDOWS=1 ;; esac
# Windows の venv の python.exe は本体を子プロセスで起動するランチャなので、Git Bash の kill では
# 子（サーバ本体）が残る。プロセスツリーごと止める。
stop() { # stop <pid>
  if [ -n "$WINDOWS" ]; then taskkill //F //T //PID "$(cat "/proc/$1/winpid" 2>/dev/null)" >/dev/null 2>&1; fi
  kill "$1" 2>/dev/null
}
cleanup() { [ -n "$PID" ] && stop "$PID"; wait "$PID" 2>/dev/null; PID=""; }
trap 'cleanup; rm -f "$LOG"' EXIT

# h2c（--http2-prior-knowledge）の確認があるので、HTTP/2 の無い curl（Windows・Git for Windows の同梱）では先に止める
if ! curl -V 2>/dev/null | grep -q HTTP2; then
  echo "FAIL curl has no HTTP/2 support ($(command -v curl)); install a curl with HTTP/2 (e.g. curl.se builds)"; exit 1
fi
free_port() { "$PYTHON" -I -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])'; }

start() { # $1 = app.py
  PORT="$(free_port)"
  VEIL_LISTEN="127.0.0.1:$PORT" "$PYTHON" "$1" >"$LOG" 2>&1 &
  PID=$!
  for _ in $(seq 1 100); do
    curl -s -o /dev/null "http://127.0.0.1:$PORT/openapi.json" && return 0
    kill -0 "$PID" 2>/dev/null || { echo "server exited:"; cat "$LOG"; return 1; }
    sleep 0.1
  done
  echo "server did not start"; cat "$LOG"; return 1
}

# check NAME EXPECT_STATUS EXPECT_BODY_SUBSTR curl-args...
check() {
  local name="$1" status="$2" sub="$3"; shift 3
  for proto in "" "--http2-prior-knowledge"; do
    local out code body
    out="$(curl -s $proto -w '\n%{http_code}' "$@")"
    code="${out##*$'\n'}"; body="${out%$'\n'*}"
    if [ "$code" = "$status" ] && [[ "$body" == *"$sub"* ]]; then ok "$name ${proto:-http1.1}"; else bad "$name ${proto:-http1.1}: status=$code body=$body (want $status / $sub)"; fi
  done
}

U() { echo "http://127.0.0.1:$PORT$1"; }
J=(-H 'content-type: application/json')

echo "== python-hello"
start "$ROOT/examples/api/python-hello/app.py" || exit 1
check ping 200 pong "$(U /ping)"
check hello 200 '"message":"Hi, w ld"' "$(U '/hello/w%20ld?greeting=Hi&times=2')"
check hello-times 200 '"times":2' "$(U '/hello/x?times=2')"
check hello-bad-int 422 '"loc":["query","times"]' "$(U '/hello/x?times=zz')"
check item 200 '"item_id":5' "$(U '/items/5?verbose=true')"
check item-404 404 'item not found' "$(U /items/500)"
check item-path-422 422 '"loc":["path","item_id"]' "$(U /items/abc)"
check user-ok 201 '"id":1' "${J[@]}" -X POST -d '{"name":"al","age":30}' "$(U /users)"
check user-age 422 '"loc":["body","age"]' "${J[@]}" -X POST -d '{"name":"al","age":200}' "$(U /users)"
check user-name 422 'at most 50' "${J[@]}" -X POST -d "{\"name\":\"$(printf 'a%.0s' $(seq 1 51))\",\"age\":3}" "$(U /users)"
check user-missing 422 '"type":"missing"' "${J[@]}" -X POST -d '{"age":3}' "$(U /users)"
check user-badjson 422 json_invalid "${J[@]}" -X POST -d '{' "$(U /users)"
check not-found 404 "" "$(U /nope)"
SPEC="$(curl -s "$(U /openapi.json)")"
for needle in '"/ping"' '"/hello/{name}"' '"/users"' '"maximum":120' '"minLength":1' '"maxLength":50' '"operationId":"create_user"'; do
  [[ "$SPEC" == *"$needle"* ]] && ok "openapi has $needle" || bad "openapi missing $needle: $SPEC"
done
[[ "$(curl -s "$(U /docs)")" == *swagger* || "$(curl -s "$(U /docs)")" == *Swagger* ]] && ok "/docs" || bad "/docs"
# 既定でアクセスログが出る（専用スレッドが最大 1 秒ごとにフラッシュする）
sleep 1.5
grep 'type=access' "$LOG" | grep -q 'path=/ping' && ok "access log by default" || bad "access log by default: $(grep -c . "$LOG") log lines"
if [ -n "$WINDOWS" ]; then
  echo "  skip graceful shutdown (SIGTERM): Windows"; stop "$PID"; wait "$PID" 2>/dev/null
else
  kill "$PID"; for _ in $(seq 1 50); do kill -0 "$PID" 2>/dev/null || break; sleep 0.1; done
  if kill -0 "$PID" 2>/dev/null; then bad "graceful shutdown (SIGTERM)"; else ok "graceful shutdown (SIGTERM)"; fi
fi
PID=""

echo "== python-fastapi-like"
start "$ROOT/examples/api/python-fastapi-like/app.py" || exit 1
HAVE_PYD="$(curl -s "$(U /pydantic)")"; echo "  ($HAVE_PYD)"
check item-404 404 'Item not found' "$(U /items/nothing)"
check create 201 '"name":"widget"' "${J[@]}" -X POST -d '{"name":"widget","price":9.5}' "$(U /items)"
check read 200 '"price":9.5' "$(U /items/widget)"
check list 200 widget "$(U '/items?limit=5&q=wid')"
check price-neg 422 '"loc":["body","price"]' "${J[@]}" -X POST -d '{"name":"x","price":-1}' "$(U /items)"
check name-missing 422 '"type":"missing"' "${J[@]}" -X POST -d '{"price":1}' "$(U /items)"
check whoami 200 '"client_ip":"127.0.0.1"' -H 'user-agent: e2e' "$(U '/whoami?a=1')"
check whoami-ua 200 '"user_agent":"e2e"' -H 'user-agent: e2e' "$(U '/whoami')"
SPEC="$(curl -s "$(U /openapi.json)")"
for needle in '"/items"' '"/items/{name}"' '"/whoami"' '"title":"Items API"' '"version":"1.0.0"' '"tags":["items"]'; do
  [[ "$SPEC" == *"$needle"* ]] && ok "openapi has $needle" || bad "openapi missing $needle"
done
if [[ "$HAVE_PYD" == *true* ]]; then
  [[ "$SPEC" == *'"Item"'* && "$SPEC" == *'"maxLength":50'* ]] && ok "openapi pydantic Item schema" || bad "openapi pydantic Item schema"
else
  [[ "$SPEC" == *'"maxLength":50'* ]] && ok "openapi body_constraints schema" || bad "openapi body_constraints schema"
fi
# Ctrl-C 相当（SIGINT）でも graceful に終了する（Windows の Git Bash からネイティブのプロセスへ SIGINT は送れない）
if [ -n "$WINDOWS" ]; then
  echo "  skip graceful shutdown (SIGINT): Windows"
else
  kill -INT "$PID"; for _ in $(seq 1 50); do kill -0 "$PID" 2>/dev/null || break; sleep 0.1; done
  if kill -0 "$PID" 2>/dev/null; then bad "graceful shutdown (SIGINT)"; else ok "graceful shutdown (SIGINT)"; fi
  PID=""
fi

echo
[ "$FAIL" -eq 0 ] && echo "ALL OK" || { echo "$FAIL FAILED"; exit 1; }
