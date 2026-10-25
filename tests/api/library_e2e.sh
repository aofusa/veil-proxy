#!/usr/bin/env bash
# API ライブラリ（F-188〜F-195）の E2E。Rust・C・Python の各サンプルを実際にビルド・起動して叩く。
#
#   tests/api/library_e2e.sh [rust|grpc|upgrade|websocket|autobahn|c|python|nonregression|external|all]   （既定 all）
#
# external（Linux のみ。all には含めない。依存を 2 回ビルドするので重い）: リポジトリの外に veil を path 依存にした
# プロジェクトを作り、`.cargo/config.toml` の `[env]` 無しと有りの両方でビルドして HTTP/1.1・HTTP/3 で応答を確かめる（F-243）。
# VEIL_EXTERNAL_SRC で依存先（既定はこのリポジトリ。公開物の確認では展開した .crate のディレクトリを渡す）を変えられる。
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
[api]
multipart_max_file_size = 209715200
upload_tmp_dir = "$W/up"
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
  mkdir -p "$W/up"
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
  run_rust_multipart "$B" "$W" "$PID"
  expect "OpenAPI paths" '"/users/{id}"' curl -s "$B/openapi.json"
  expect "OpenAPI constraints" '"maxLength":50' curl -s "$B/openapi.json"
  expect "Swagger UI" "swagger-ui-bundle.js" curl -s "$B/docs"
  expect "404 for unknown" "404" curl -s -o /dev/null -w "%{http_code}" "$B/nope"
  expect "L4 TCP echo" "hello-tcp" l4_echo tcp "$L4" hello-tcp
  expect "L4 UDP echo" "echo:hi" l4_echo udp "$U" hi
  expect "threads default = CPU count" "workers=$(ncpu)" cat "$W/log"
  # SIGHUP: ファイルを読み直しても組み込みのルート（マクロ）とコード側の上書きは保たれる
  if [ "$OS" != Windows ]; then  # Windows に SIGHUP は無い
    sed -i.bak 's/level = "info"/level = "warn"/' "$W/config.toml"; kill -HUP "$PID"; sleep 1
    expect "after SIGHUP routes kept" "Hello, World!" curl -s "$B/hello"
  fi
  kill "$PID"; for _ in $(seq 1 50); do kill -0 "$PID" 2>/dev/null || break; sleep 0.2; done
  kill -0 "$PID" 2>/dev/null && bad "graceful shutdown" || ok "graceful shutdown (SIGTERM)"
  # config.toml が無い場合はライブラリ既定（listen はコードの VEIL_LISTEN 上書きで変える）
  local W2; W2="$(mktemp -d)"; local P2; P2=$(free_port)
  (cd "$W2" && VEIL_LISTEN="127.0.0.1:$P2" exec "$T/debug/veil-example-showcase$EXE") > "$W2/log" 2> "$W2/err" &
  local PID2=$!; PIDS+=("$PID2")
  wait_port "$P2" && expect "no config.toml -> library defaults" "Hello, World!" curl -s "http://127.0.0.1:$P2/hello" || bad "no-config start"
  expect "defaults are plaintext h2c-only + HTTP/1.1" "cleartext http1=on h2c=on" cat "$W2/log"
  # F-244: ライブラリの既定はアクセスログを標準出力に出す（専用スレッドが最大 1 秒ごとにフラッシュする）
  sleep 1.5
  expect "access log on stdout by default" "path=/hello" bash -c "grep -a 'type=access' '$W2/log'"
  # 環境変数 VEIL_OVERRIDE_<n> もライブラリで効く
  # 別のポートで起動する（同じポートだと SO_REUSEPORT で終了処理中の前のサーバと並んで待ち受け、
  # wait_port が前のサーバで通ってしまう。以前は P3 を取りながら P2 を使っていた）
  kill "$PID2"; for _ in $(seq 1 50); do kill -0 "$PID2" 2>/dev/null || break; sleep 0.2; done
  local P3; P3=$(free_port)
  (cd "$W2" && VEIL_LISTEN="127.0.0.1:$P3" VEIL_OVERRIDE_1="api.openapi_enabled = false" VEIL_OVERRIDE_2="access_log.enabled = false" exec "$T/debug/veil-example-showcase$EXE") > "$W2/log3" 2>&1 &
  PIDS+=("$!"); wait_port "$P3" "$!"
  expect "VEIL_OVERRIDE disables OpenAPI" "404" curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:$P3/openapi.json"
  curl -s -o /dev/null "http://127.0.0.1:$P3/hello"; sleep 1.5
  if grep -a -q 'type=access' "$W2/log3"; then bad "access_log.enabled = false stops the access log"; else ok "access_log.enabled = false stops the access log"; fi
  kill "$!" 2>/dev/null
}

run_rust_multipart() { # F-266: multipart/form-data（examples/api/rust-showcase の /profile・/files-upload・/upload-stream）
  local B="$1" W="$2" PID="$3"
  printf 'PNGDATA' > "$W/a.png"; printf 'one' > "$W/d1.txt"; printf 'second' > "$W/d2.txt"
  expect "multipart form + file" '"avatar":"a.png","avatar_size":7,"avatar_type":"image/png","banner_size":null,"name":"al"' \
    curl -s -F name=al -F age=30 -F "avatar=@$W/a.png;type=image/png" "$B/profile"
  expect "multipart optional file" '"banner_size":3' curl -s -F name=al -F age=30 -F "avatar=@$W/a.png" -F "banner=@$W/d1.txt" "$B/profile"
  expect "multipart validation 422" '"less_than_equal"' curl -s -F name=al -F age=500 -F "avatar=@$W/a.png" "$B/profile"
  expect "multipart missing file 422" '"loc":["body","avatar"]' curl -s -F name=al -F age=3 "$B/profile"
  expect "multipart several files" '"sizes":[3,6]' curl -s -F "docs=@$W/d1.txt" -F "docs=@$W/d2.txt" "$B/files-upload"
  expect "multipart malformed 400" "400" curl -s -o /dev/null -w "%{http_code}" -H 'content-type: multipart/form-data; boundary=x' --data-binary 'nope' "$B/files-upload"
  expect "multipart (h2c)" '"sizes":[3,6]' curl -s --http2-prior-knowledge -F "docs=@$W/d1.txt" -F "docs=@$W/d2.txt" "$B/files-upload"
  expect "streaming multipart (HTTP/1.1)" '{"parts":[{"name":"t","value":"v"},{"file":"d2.txt","name":"f","size":6}]}' \
    curl -s -F t=v -F "f=@$W/d2.txt" "$B/upload-stream"
  expect "streaming multipart (h2c)" '"size":6' curl -s --http2-prior-knowledge -F t=v -F "f=@$W/d2.txt" "$B/upload-stream"
  # 100 MB のファイル: 本文を溜めずに一時ファイルへ書く（常駐メモリが増えない）
  dd if=/dev/urandom of="$W/big.bin" bs=1048576 count=100 2>/dev/null
  local rss0 rss1
  rss0=$(ps -o rss= -p "$PID" 2>/dev/null | tr -d ' ')
  expect "streaming multipart 100 MB (HTTP/1.1)" '"size":104857600' curl -s -F "big=@$W/big.bin" "$B/upload-stream"
  expect "streaming multipart 100 MB (h2c)" '"size":104857600' curl -s --http2-prior-knowledge -F "big=@$W/big.bin" "$B/upload-stream"
  rss1=$(ps -o rss= -p "$PID" 2>/dev/null | tr -d ' ')
  if [ -n "$rss0" ] && [ -n "$rss1" ]; then
    [ $((rss1 - rss0)) -lt 51200 ] && ok "100 MB uploads keep RSS (+$(( (rss1 - rss0) / 1024 )) MiB)" || bad "100 MB uploads grew RSS by $(( (rss1 - rss0) / 1024 )) MiB"
  fi
  rm -f "$W/big.bin"
  [ -z "$(ls -A "$W/up")" ] && ok "no temporary upload files remain" || bad "temporary upload files remain: $(ls "$W/up")"
  expect "OpenAPI multipart body" '"format":"binary"' bash -c "curl -s '$B/openapi.json' | $PY3 -I -c 'import json,sys; print(json.dumps(json.load(sys.stdin)[\"paths\"][\"/profile\"][\"post\"][\"requestBody\"][\"content\"][\"multipart/form-data\"], separators=(\",\",\":\")))'"
}

run_python_multipart() { # F-266: Python の Form・File・UploadFile（examples/api/python-features の /upload・/upload-many・/login）
  local B="$1" W="$2" PID="$3"
  printf 'hello' > "$W/s.txt"; printf 'n1' > "$W/n.txt"
  expect "py multipart form + small file (memory)" '"title":"t","count":2,"filename":"s.txt","size":5,"sha256":"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824","where":"memory","note":null' \
    curl -s -F title=t -F count=2 -F "doc=@$W/s.txt" "$B/upload"
  expect "py multipart optional file" '"note":"n1"' curl -s -F title=t -F "doc=@$W/s.txt" -F "note=@$W/n.txt" "$B/upload"
  expect "py multipart (h2c)" '"where":"memory"' curl -s --http2-prior-knowledge -F title=t -F "doc=@$W/s.txt" "$B/upload"
  expect "py multipart missing file 422" '"loc":["body","doc"]' curl -s -F title=t "$B/upload"
  expect "py multipart bad int 422" '"loc":["body","count"]' curl -s -F title=t -F count=x -F "doc=@$W/s.txt" "$B/upload"
  expect "py multipart malformed 400" "400" curl -s -o /dev/null -w "%{http_code}" -H 'content-type: multipart/form-data; boundary=x' --data-binary 'nope' "$B/upload"
  expect "py several files" '[{"filename":"s.txt","size":5},{"filename":"n.txt","size":2}]' curl -s -F "docs=@$W/s.txt" -F "docs=@$W/n.txt" "$B/upload-many"
  expect "py urlencoded form" '{"user":"a b","remember":true}' curl -s -d 'user=a%20b&remember=on' "$B/login"
  # 100 MB のファイル: Rust が一時ファイルへ書き、Python は fd で読む（常駐メモリが増えない）
  dd if=/dev/urandom of="$W/big.bin" bs=1048576 count=100 2>/dev/null
  local sum rss0 rss1
  sum=$("$PY3" -I -c 'import hashlib,sys; print(hashlib.sha256(open(sys.argv[1],"rb").read()).hexdigest())' "$W/big.bin")
  rss0=$(ps -o rss= -p "$PID" 2>/dev/null | tr -d ' ')
  expect "py 100 MB upload (HTTP/1.1, disk)" "\"size\":104857600,\"sha256\":\"$sum\",\"where\":\"disk\"" curl -s -F title=b -F "doc=@$W/big.bin" "$B/upload"
  expect "py 100 MB upload (h2c)" "\"sha256\":\"$sum\"" curl -s --http2-prior-knowledge -F title=b -F "doc=@$W/big.bin" "$B/upload"
  rss1=$(ps -o rss= -p "$PID" 2>/dev/null | tr -d ' ')
  if [ -n "$rss0" ] && [ -n "$rss1" ]; then
    [ $((rss1 - rss0)) -lt 51200 ] && ok "py 100 MB uploads keep RSS (+$(( (rss1 - rss0) / 1024 )) MiB)" || bad "py 100 MB uploads grew RSS by $(( (rss1 - rss0) / 1024 )) MiB"
  fi
  rm -f "$W/big.bin"
  expect "py OpenAPI multipart body" '"format":"binary"' bash -c "curl -s '$B/openapi.json' | $PY3 -I -c 'import json,sys; print(json.dumps(json.load(sys.stdin)[\"paths\"][\"/upload\"][\"post\"][\"requestBody\"][\"content\"][\"multipart/form-data\"], separators=(\",\",\":\")))'"
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
    # ログは実行ファイルごとに分ける（前のプロセスが終わる前に同じファイルを切り詰めると、残った書き込みで NUL の穴が空く）
    local P; P=$(free_port)
    VEIL_LISTEN="127.0.0.1:$P" PATH="$ROOT/target/release:$PATH" "$CH/$bin$EXE" > "$TMPD/c-$bin.log" 2>&1 &
    local PID=$!; PIDS+=("$PID")
    wait_port "$P" "$PID" || { bad "$bin start"; cat "$TMPD/c-$bin.log"; continue; }
    local B="http://127.0.0.1:$P"
    expect "$bin GET /" "Hello from C" curl -s "$B/"
    expect "$bin path param (decoded)" '{"id":"a b"}' curl -s "$B/users/a%20b"
    expect "$bin POST echo" "xyz" curl -s -X POST --data xyz "$B/echo"
    expect "$bin inline route h2c" "pong" curl -s --http2-prior-knowledge "$B/ping"
    expect "$bin OpenAPI from C metadata" '"summary":"Get a user"' curl -s "$B/openapi.json"
    sleep 1.5
    # -a: ログに想定外のバイトがあっても grep が「Binary file matches」だけを返さないように（FreeBSD x86_64 で起きた）
    expect "$bin access log by default" "path=/users/" bash -c "grep -a 'type=access' '$TMPD/c-$bin.log'"
    # ログに NUL などの制御文字（改行・タブ・ESC 以外）が混ざっていないこと（混ざっていれば中身を出す）
    if LC_ALL=C grep -q -P '[\x00-\x08\x0b\x0c\x0e-\x1a\x1c-\x1f]' "$TMPD/c-$bin.log" 2>/dev/null \
       || [ "$(LC_ALL=C tr -d -c '\000' < "$TMPD/c-$bin.log" | wc -c)" -gt 0 ]; then
      bad "$bin log has control bytes: $(od -c "$TMPD/c-$bin.log" | grep -n -E '\\0|[0-9]{3}' | head -3 | tr '\n' ' ')"
    else
      ok "$bin log is plain text"
    fi
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

c_msvc_dyn() { # c_msvc_dyn <dir> <name>（<name>.c を veil.dll とリンクして <name>.exe）
  local dir="$1" name="$2" vc="${VCVARS:-}"
  if [ -z "$vc" ]; then
    local vs; vs=$("/c/Program Files (x86)/Microsoft Visual Studio/Installer/vswhere.exe" -latest -property installationPath | tr -d '\r')
    vc="$vs\\VC\\Auxiliary\\Build\\vcvars64.bat"
  fi
  local rel; rel="$(cygpath -w "$ROOT/target/release")"
  local inc; inc="$(cygpath -w "$ROOT/crates/veil-capi/include")"
  cat > "$dir/build-msvc.bat" <<BAT
@call "$vc" >nul || exit /b 1
cl /nologo /W3 /utf-8 /I"$inc" $name.c /Fe:$name.exe /link /LIBPATH:"$rel" veil.dll.lib || exit /b 1
BAT
  (cd "$dir" && cmd //c build-msvc.bat) > "$TMPD/msvc.log" 2>&1 || { tail -20 "$TMPD/msvc.log"; rm -f "$dir/build-msvc.bat"; return 1; }
  rm -f "$dir/build-msvc.bat" "$dir"/*.obj
}

run_c_features() {
  echo "== C (examples/api/c-features: ストリーミング・gRPC・L4、F-210)"
  local CF="$ROOT/examples/api/c-features"
  rm -f "$CF/features" "$CF/features.exe"
  if [ "$OS" = Windows ]; then
    c_msvc_dyn "$CF" features || { bad "compile features.c (MSVC)"; return; }
  else
    local MK; MK=$(command -v gmake || command -v make)
    local libs="-lpthread -ldl -lm"
    [ "$OS" = Linux ] || libs=$(cd "$ROOT" && cargo rustc -q -p veil-capi --release --crate-type staticlib -- \
      --print native-static-libs 2>&1 | sed -n 's/.*native-static-libs: //p' | tail -1)
    "$MK" -s -C "$CF" CC="${CC:-cc}" LIBS="$libs" features >/dev/null || { bad "compile features.c"; return; }
  fi
  local W; W="$(mktemp -d)"; [ "$OS" = Windows ] && W="$(cygpath -m "$W")"
  MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*' openssl req -x509 -newkey rsa:2048 -nodes -keyout "$W/key.pem" -out "$W/cert.pem" -days 2 \
    -subj /CN=localhost -addext subjectAltName=DNS:localhost,IP:127.0.0.1 2>/dev/null || { bad "cert"; return; }
  local P S L4 U; P=$(free_port); S=$(free_port); L4=$(free_port); U=$(free_port)
  (cd "$W" && VEIL_LISTEN="127.0.0.1:$P" VEIL_TLS_LISTEN="127.0.0.1:$S" VEIL_TLS_CERT="$W/cert.pem" VEIL_TLS_KEY="$W/key.pem" \
    VEIL_L4_TCP="127.0.0.1:$L4" VEIL_L4_UDP="127.0.0.1:$U" PATH="$ROOT/target/release:$PATH" exec "$CF/features$EXE") > "$W/log" 2>&1 &
  local PID=$!; PIDS+=("$PID")
  wait_port "$P" "$PID" || { bad "features start"; cat "$W/log"; return; }
  local B="http://127.0.0.1:$P"
  expect "C streaming response (SSE)" "data: tick 3" curl -s -N "$B/sse"
  expect "C streaming trailer (h2c)" "x-ticks: 3" bash -c "curl -s -i --http2-prior-knowledge '$B/sse' | tr A-Z a-z"
  expect "C streaming request body (1MiB)" "1048576" bash -c "dd if=/dev/zero bs=1024 count=1024 2>/dev/null | curl -s -X POST --data-binary @- '$B/count'"
  expect "C streaming request body (h2c)" "300000" bash -c "dd if=/dev/zero bs=1000 count=300 2>/dev/null | curl -s --http2-prior-knowledge -X POST --data-binary @- '$B/count'"
  expect "C L4 TCP echo" "c-tcp" l4_echo tcp "$L4" c-tcp
  # F-261: C のハンドラのアップグレード（101 の後は L4 と同じコールバック。ヘッダと同じパケットの行も届く）
  expect "C upgrade (101 + L4 callbacks)" "hello|c-up|x-from: c" "$PY3" -I -c '
import socket, sys
s = socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=5)
s.sendall(b"GET /upgrade HTTP/1.1\r\nHost: x\r\nConnection: upgrade\r\nUpgrade: x-echo\r\n\r\nc-up")
d = b""
while not d.endswith(b"c-up"):
    b = s.recv(4096)
    if not b: break
    d += b
head, _, body = d.partition(b"\r\n\r\n")
ok = head.startswith(b"HTTP/1.1 101 ") and b"x-from: c" in head
print(body.decode().replace("\n", "|") + ("|x-from: c" if ok else "|bad head " + head.decode()))' "$P"
  expect "C upgrade on h2c is 400" "400" curl -s -o /dev/null -w "%{http_code}" --http2-prior-knowledge "$B/upgrade"
  # F-262: C の WebSocket（veil_ws_accept。コールバックはハンドラ用プール）
  "$PY3" -I "$ROOT/tests/api/ws_check.py" basic "$P" /ws | sed "s|\$| (C)|; s|^|  |" | tee "$W/wsr"
  grep -q "FAIL" "$W/wsr" && FAIL=1
  "$PY3" -I "$ROOT/tests/api/ws_check.py" many "$P" 200 | sed "s|\$| (C)|; s|^|  |" | tee "$W/wsr"
  grep -q "^  ok" "$W/wsr" || FAIL=1
  expect "C L4 UDP echo" "echo:c-udp" l4_echo udp "$U" c-udp
  (cd "$ROOT/tests/api/grpc_client" && CARGO_TARGET_DIR="$ROOT/target/examples-api" cargo build -q) || { bad "build grpc client"; return; }
  local out; out="$(tmo 300 "$ROOT/target/examples-api/debug/veil-grpc-check$EXE" --no-reflection --h2c "127.0.0.1:$P" --h3 "127.0.0.1:$S" 2>&1)"
  local rc=$?
  echo "$out" | sed 's/^/  /'
  [ "$rc" = 0 ] && ok "C gRPC interop (tonic + HTTP/3, exit 0)" || bad "C gRPC interop (exit $rc)"
  run_c_multipart "$B" "$W" "$PID" "$S"
  kill "$PID"
}

run_c_multipart() { # F-266: C の veil_multipart_next（VEIL_ROUTE_FORM の /upload と溜めた本文の /parse）
  local B="$1" W="$2" PID="$3" S="$4"
  printf 'hello' > "$W/s.txt"
  # バイトの総和: hello = 532、v = 118
  expect "C multipart (form route, memory)" "t=1/118(mem);f:s.txt=5/532(mem);" curl -s -F t=v -F "f=@$W/s.txt" "$B/upload"
  expect "C multipart (buffered body)" "f:s.txt=5/532(mem);" curl -s -F "f=@$W/s.txt" "$B/parse"
  expect "C multipart (h2c)" "f:s.txt=5/532(mem);" curl -s --http2-prior-knowledge -F "f=@$W/s.txt" "$B/upload"
  expect "C multipart malformed 400 (form route)" "400" curl -s -o /dev/null -w "%{http_code}" -H 'content-type: multipart/form-data; boundary=x' --data-binary 'nope' "$B/upload"
  expect "C multipart malformed 400 (buffered)" "400" curl -s -o /dev/null -w "%{http_code}" -H 'content-type: multipart/form-data; boundary=x' --data-binary $'--x\r\nbroken' "$B/parse"
  dd if=/dev/urandom of="$W/big.bin" bs=1048576 count=100 2>/dev/null
  local h rss0 rss1
  h=$("$PY3" -I -c 'import sys; print(sum(open(sys.argv[1], "rb").read()))' "$W/big.bin")
  rss0=$(ps -o rss= -p "$PID" 2>/dev/null | tr -d ' ')
  expect "C 100 MB upload (HTTP/1.1, temp file)" "big:big.bin=104857600/$h(disk);" curl -s -F "big=@$W/big.bin" "$B/upload"
  expect "C 100 MB upload (h2c)" "big:big.bin=104857600/$h(disk);" curl -s --http2-prior-knowledge -F "big=@$W/big.bin" "$B/upload"
  rss1=$(ps -o rss= -p "$PID" 2>/dev/null | tr -d ' ')
  if [ -n "$rss0" ] && [ -n "$rss1" ]; then
    [ $((rss1 - rss0)) -lt 51200 ] && ok "C 100 MB uploads keep RSS (+$(( (rss1 - rss0) / 1024 )) MiB)" || bad "C 100 MB uploads grew RSS by $(( (rss1 - rss0) / 1024 )) MiB"
  fi
  rm -f "$W/big.bin"
  # HTTP/3（curl に HTTP/3 が無いので quinn + h3 の試験クライアントで送る）: 30 MB は [api] max_body_size（10 MiB）を超える
  local sum3; sum3=$("$PY3" -I -c '
import os, sys
data = os.urandom(30 << 20)
body = (b"--BND\r\ncontent-disposition: form-data; name=\"t\"\r\n\r\nv\r\n--BND\r\n"
        b"content-disposition: form-data; name=\"big\"; filename=\"b.bin\"\r\n\r\n" + data + b"\r\n--BND--\r\n")
open(sys.argv[1], "wb").write(body)
print(sum(data))' "$W/mp.bin")
  expect "C multipart 30 MB (HTTP/3)" "t=1/118(mem);big:b.bin=31457280/$sum3(disk);" \
    "$ROOT/target/examples-api/debug/veil-grpc-check$EXE" --h3-fetch "127.0.0.1:$S" POST /upload -H 'content-type: multipart/form-data; boundary=BND' --data-file "$W/mp.bin"
  rm -f "$W/mp.bin"
}

run_python() {
  echo "== Python (crates/veil-python)"
  local PY="${PYTHON:-}"
  if [ -z "$PY" ]; then
    local V="$ROOT/target/py-venv"
    local VB="$V/bin"; [ "$OS" = Windows ] && VB="$V/Scripts"
    [ -x "$VB/python$EXE" ] || "$PY3" -m venv "$V"
    "$VB/python$EXE" -m pip -q install maturin >/dev/null
    if ! (cd "$ROOT/crates/veil-python" && unset CONDA_PREFIX && VIRTUAL_ENV="$V" PATH="$VB:$PATH" maturin develop -q --release) > "$TMPD/maturin.log" 2>&1; then
      # maturin の wheel の platform タグと pip が受け付けるタグが違う OS がある（NetBSD aarch64 は maturin が
      # netbsd_10_1_arm64、pip は evbarm 系）。wheel を作り、pip が受け付けるタグに付け直して入れる
      if grep -q "is not a supported wheel on this platform" "$TMPD/maturin.log"; then
        local tag
        tag=$("$VB/python$EXE" -c 'from pip._vendor.packaging import tags; print(next(iter(tags.sys_tags())).platform)')
        "$VB/python$EXE" -m pip -q install wheel >/dev/null
        rm -rf "$TMPD/wheels"
        (cd "$ROOT/crates/veil-python" && unset CONDA_PREFIX && VIRTUAL_ENV="$V" PATH="$VB:$PATH" maturin build -q --release -o "$TMPD/wheels") >> "$TMPD/maturin.log" 2>&1 \
          && "$VB/python$EXE" -m wheel tags --remove --platform-tag "$tag" "$TMPD"/wheels/*.whl >> "$TMPD/maturin.log" 2>&1 \
          && "$VB/python$EXE" -m pip -q install --force-reinstall "$TMPD"/wheels/*"$tag"*.whl >> "$TMPD/maturin.log" 2>&1 \
          || { bad "maturin develop (retag $tag)"; tail -20 "$TMPD/maturin.log"; return; }
      else
        bad "maturin develop"; tail -20 "$TMPD/maturin.log"; return
      fi
    fi
    PY="$VB/python$EXE"
  fi
  (cd "$ROOT/crates/veil-python" && "$PY" -I tests/test_binding.py) >/dev/null 2>&1 && ok "unit tests" || bad "unit tests"
  (cd "$ROOT/crates/veil-python" && "$PY" -I tests/test_native.py) >/dev/null 2>&1 && ok "native request / fast call (F-212)" || bad "native request / fast call"
  (cd "$ROOT/crates/veil-python" && PYTHON="$PY" bash tests/e2e.sh) > "$TMPD/py.log" 2>&1 && ok "e2e.sh (both examples)" || { bad "e2e.sh"; grep -E "FAIL|server" "$TMPD/py.log"; }
  run_python_f210 "$PY"
  run_python_ws "$PY"
}

run_python_ws() { # F-263: 常駐のイベントループ・WebSocket・アップグレード・lifespan（examples/api/python-websocket）
  local PY="$1"
  echo "== Python: event loop / WebSocket / upgrade / lifespan (F-263)"
  (cd "$ROOT/crates/veil-python" && "$PY" -I tests/test_aio.py) > "$TMPD/aio.log" 2>&1 && ok "py test_aio.py" || { bad "py test_aio.py"; tail -20 "$TMPD/aio.log"; }
  "$PY" -m pip -q install pydantic >/dev/null 2>&1 || echo "  skip py /typed: pydantic is not installable here"
  local W; W="$(mktemp -d)"; [ "$OS" = Windows ] && W="$(cygpath -m "$W")"
  local P; P=$(free_port)
  VEIL_LISTEN="127.0.0.1:$P" "$PY" "$ROOT/examples/api/python-websocket/app.py" > "$W/log" 2>&1 &
  local PID=$!; PIDS+=("$PID")
  wait_port "$P" "$PID" || { bad "python-websocket start"; cat "$W/log"; return; }
  "$PY3" -I "$ROOT/tests/api/ws_check.py" python "$P" | sed "s|^|  |" | tee "$W/r"
  grep -q "FAIL" "$W/r" && FAIL=1
  "$PY3" -I "$ROOT/tests/api/ws_check.py" many "$P" "${WS_E2E_CONNS:-2000}" | sed "s|^|  py |" | tee "$W/r"
  grep -q "^  py ok" "$W/r" || FAIL=1
  # 常駐のループ: await asyncio.sleep(1) を同時に（既定 2,000 本。PY_E2E_SLEEPERS=32768 で多くの接続を）
  "$PY3" -I "$ROOT/tests/api/ws_check.py" sleepers "$P" "${PY_E2E_SLEEPERS:-2000}" | sed "s|^|  py |" | tee "$W/r"
  grep -q "^  py ok" "$W/r" || FAIL=1
  expect "py OpenAPI lists /ws with x-veil-websocket" "True ['echo.v1'] False" bash -c "curl -s 'http://127.0.0.1:$P/openapi.json' | $PY3 -I -c 'import json,sys; ps=json.load(sys.stdin)[\"paths\"]; o=ps[\"/ws\"][\"get\"]; print(\"101\" in o[\"responses\"], o[\"x-veil-websocket\"][\"subprotocols\"], \"/hidden\" in ps)'"
  expect "py WebSocket on h2c is 400" "400" curl -s -o /dev/null -w "%{http_code}" --http2-prior-knowledge "http://127.0.0.1:$P/ws"
  if [ "$OS" != Windows ]; then
    "$PY3" -I "$ROOT/tests/api/ws_check.py" hold "$P" 20 > "$W/hold" 2>&1 &
    local HP=$!
    for _ in $(seq 1 600); do grep -q held "$W/hold" && break; sleep 0.1; done
    kill "$PID"; wait "$HP"
    grep -q "close 1001" "$W/hold" && ok "py graceful shutdown sends Close 1001" || bad "py graceful shutdown sends Close 1001: $(cat "$W/hold")"
    for _ in $(seq 1 100); do kill -0 "$PID" 2>/dev/null || break; sleep 0.2; done
    grep -q "lifespan: shutdown" "$W/log" && ok "py lifespan shutdown runs after the server stopped" || bad "py lifespan shutdown: $(tail -5 "$W/log")"
  fi
  py_stop "$PID" 2>/dev/null
  # lifespan の開始で例外なら起動しない（終了コード 1）
  printf 'import contextlib, veil\n@contextlib.asynccontextmanager\nasync def ls(app):\n    raise RuntimeError("no db")\n    yield\napp = veil.App(lifespan=ls)\napp.run(listen_cleartext="127.0.0.1:%s")\n' "$(free_port)" > "$W/bad.py"
  tmo 60 "$PY" "$W/bad.py" > "$W/bad.log" 2>&1
  local rc=$?
  [ "$rc" = 1 ] && grep -q "no db" "$W/bad.log" && ok "py lifespan startup failure exits 1" || bad "py lifespan startup failure (exit $rc): $(tail -3 "$W/bad.log")"
  run_python_autobahn "$PY"
}

run_python_autobahn() { # Autobahn TestSuite の 1.*〜7.*（Python の /ws のエコー）
  local PY="$1"
  if ! command -v docker >/dev/null || ! docker image inspect crossbario/autobahn-testsuite >/dev/null 2>&1; then
    echo "  skip py Autobahn TestSuite (docker or crossbario/autobahn-testsuite is missing)"
    return
  fi
  mkdir -p "$ROOT/target"
  local W; W="$(mktemp -d "$ROOT/target/autobahn-py.XXXXXX")"
  local P; P=$(free_port)
  VEIL_LISTEN="127.0.0.1:$P" "$PY" "$ROOT/examples/api/python-websocket/app.py" > "$W/log" 2>&1 &
  local PID=$!; PIDS+=("$PID")
  wait_port "$P" "$PID" || { bad "python-websocket start (autobahn)"; cat "$W/log"; return; }
  cat > "$W/fuzzingclient.json" <<JSON
{ "outdir": "/w/reports", "servers": [ {"agent": "veil-python", "url": "ws://127.0.0.1:$P/ws"} ],
  "cases": ["1.*", "2.*", "3.*", "4.*", "5.*", "6.*", "7.*"], "exclude-cases": [], "exclude-agent-cases": {} }
JSON
  tmo 3600 docker run --rm --network host --user "$(id -u):$(id -g)" -v "$W:/w" crossbario/autobahn-testsuite \
    wstest -m fuzzingclient -s /w/fuzzingclient.json > "$W/autobahn.log" 2>&1 || { bad "py Autobahn fuzzingclient run"; tail -20 "$W/autobahn.log"; }
  autobahn_summary "$W/reports/index.json" | tee "$W/r"
  grep -q "^  ok" "$W/r" && ! grep -q "FAIL" "$W/r" || FAIL=1
  py_stop "$PID" 2>/dev/null
  rm -rf "$W"
}

autobahn_summary() { # autobahn_summary <index.json>
  "$PY3" -I - "$1" <<'PY' | sed "s|^|  |"
import json, sys
idx = json.load(open(sys.argv[1]))
for agent, cases in sorted(idx.items()):
    # 13.* の UNIMPLEMENTED は「申し出のパラメータ（サーバの窓を 15 より小さく）を断った」もので、RFC 7692 で許される
    def good(c, r):
        ok = ("OK", "NON-STRICT", "INFORMATIONAL") + (("UNIMPLEMENTED",) if c.startswith("13.") else ())
        return r["behavior"] in ok and r["behaviorClose"] in ("OK", "INFORMATIONAL")
    badc = [(c, r["behavior"], r["behaviorClose"]) for c, r in cases.items() if not good(c, r)]
    unimpl = sum(1 for r in cases.values() if r["behavior"] == "UNIMPLEMENTED")
    if badc:
        print(f"FAIL Autobahn {agent}: {len(badc)} of {len(cases)} cases failed: {badc[:20]}")
    else:
        print(f"ok   Autobahn {agent}: {len(cases)} cases OK / NON-STRICT / INFORMATIONAL ({unimpl} UNIMPLEMENTED in 13.*)")
PY
}

py_stop() { # py_stop <pid>（Windows の venv の python はランチャなので子ごと止める）
  if [ "$OS" = Windows ]; then taskkill //F //T //PID "$(cat /proc/$1/winpid 2>/dev/null || echo $1)" >/dev/null 2>&1 || kill "$1"
  else kill "$1"; fi
}

run_python_f210() { # ストリーミング・L4・gRPC（基本の層と grpcio の層）・GraphQL（F-210）
  local PY="$1" EX="$ROOT/examples/api"
  echo "== Python: streaming / L4 / gRPC / GraphQL (F-210)"
  local W; W="$(mktemp -d)"; [ "$OS" = Windows ] && W="$(cygpath -m "$W")"
  local P L4 U; P=$(free_port); L4=$(free_port); U=$(free_port)
  VEIL_LISTEN="127.0.0.1:$P" VEIL_L4_TCP="127.0.0.1:$L4" VEIL_L4_UDP="127.0.0.1:$U" "$PY" "$EX/python-features/app.py" > "$W/f.log" 2>&1 &
  local PID=$!; PIDS+=("$PID")
  if wait_port "$P" "$PID"; then
    local B="http://127.0.0.1:$P"
    expect "py generator streaming" "line 3" curl -s -N "$B/stream"
    expect "py SSE" 'data: {"n":3}' curl -s -N "$B/sse"
    expect "py async generator + trailer (h2c)" "x-parts: 3" bash -c "curl -s -i --http2-prior-knowledge '$B/async-stream' | tr A-Z a-z"
    expect "py streaming request body (1MiB)" "1048576" bash -c "dd if=/dev/zero bs=1024 count=1024 2>/dev/null | curl -s -X POST --data-binary @- '$B/count'"
    expect "py L4 TCP echo" "py-tcp" l4_echo tcp "$L4" py-tcp
    expect "py L4 UDP echo" "echo:py-udp" l4_echo udp "$U" py-udp
    run_python_multipart "$B" "$W" "$PID"
  else
    bad "python-features start"; cat "$W/f.log"
  fi
  py_stop "$PID"
  # gRPC: メッセージの生成は grpc_tools（入れば）か protoc + protobuf
  "$PY" -m pip -q install protobuf >/dev/null 2>&1
  local GEN="$W/gen"; mkdir -p "$GEN"; local have_grpcio=0
  if "$PY" -m pip -q install grpcio grpcio-tools >/dev/null 2>&1 && \
     "$PY" -m grpc_tools.protoc -I "$EX/rust-grpc/proto" --python_out="$GEN" --grpc_python_out="$GEN" "$EX/rust-grpc/proto/demo.proto" 2>/dev/null; then
    have_grpcio=1
  else
    "${PROTOC:-protoc}" -I "$EX/rust-grpc/proto" --python_out="$GEN" "$EX/rust-grpc/proto/demo.proto" || { bad "generate demo_pb2"; return; }
  fi
  MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*' openssl req -x509 -newkey rsa:2048 -nodes -keyout "$W/key.pem" -out "$W/cert.pem" -days 2 \
    -subj /CN=localhost -addext subjectAltName=DNS:localhost,IP:127.0.0.1 2>/dev/null || { bad "cert"; return; }
  (cd "$ROOT/tests/api/grpc_client" && CARGO_TARGET_DIR="$ROOT/target/examples-api" cargo build -q) || { bad "build grpc client"; return; }
  local modes="plain"; [ "$have_grpcio" = 1 ] && modes="plain grpcio"
  local mode
  for mode in $modes; do
    local GP GS; GP=$(free_port); GS=$(free_port)
    GEN_DIR="$GEN" VEIL_GRPC_MODE=$mode VEIL_LISTEN="127.0.0.1:$GP" VEIL_TLS_LISTEN="127.0.0.1:$GS" VEIL_TLS_CERT="$W/cert.pem" \
      VEIL_TLS_KEY="$W/key.pem" "$PY" "$EX/python-grpc/server.py" > "$W/g.log" 2>&1 &
    local GPID=$!; PIDS+=("$GPID")
    wait_port "$GP" "$GPID" || { bad "python-grpc ($mode) start"; cat "$W/g.log"; continue; }
    local out; out="$(tmo 300 "$ROOT/target/examples-api/debug/veil-grpc-check$EXE" --no-reflection --h2c "127.0.0.1:$GP" --h3 "127.0.0.1:$GS" 2>&1)"
    local rc=$?
    echo "$out" | sed 's/^/  /'
    [ "$rc" = 0 ] && ok "py gRPC interop ($mode)" || bad "py gRPC interop ($mode, exit $rc)"
    py_stop "$GPID"
  done
  [ "$have_grpcio" = 1 ] || echo "  skip py gRPC (grpcio layer): grpcio is not installable here"
  # GraphQL（strawberry。入らない環境では飛ばす）
  if "$PY" -m pip -q install strawberry-graphql >/dev/null 2>&1; then
    local QP; QP=$(free_port)
    VEIL_LISTEN="127.0.0.1:$QP" "$PY" "$EX/python-graphql/app.py" > "$W/q.log" 2>&1 &
    local QPID=$!; PIDS+=("$QPID")
    if wait_port "$QP" "$QPID"; then
      local QB="http://127.0.0.1:$QP/graphql" J='content-type: application/json'
      expect "py GraphQL query" '{"data":{"add":3}}' curl -s -X POST "$QB" -H "$J" -d '{"query":"{ add(a:1,b:2) }"}'
      expect "py GraphQL mutation" '{"data":{"increment":5}}' curl -s -X POST "$QB" -H "$J" -d '{"query":"mutation { increment(by:5) }"}'
      expect "py GraphQL subscription (SSE)" "event: complete" curl -s -N -X POST "$QB" -H "$J" -H 'accept: text/event-stream' -d '{"query":"subscription { countdown(start: 2) }"}'
    else
      bad "python-graphql start"; cat "$W/q.log"
    fi
    py_stop "$QPID"
  else
    echo "  skip py GraphQL: strawberry-graphql is not installable here"
  fi
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
    VEIL_TLS_KEY="$W/key.pem" VEIL_OVERRIDE_1='api.grpc_web_cors_origins = ["https://app.example"]' \
    exec "$T/debug/veil-example-grpc$EXE") > "$W/log" 2>&1 &
  local PID=$!; PIDS+=("$PID")
  wait_port "$P" "$PID" || { bad "start ($label)"; cat "$W/log"; return; }
  local out; out="$(tmo 300 "$ROOT/target/examples-api/debug/veil-grpc-check$EXE" --h2c "127.0.0.1:$P" --h3 "127.0.0.1:$S" 2>&1)"
  local rc=$?
  echo "$out" | sed 's/^/  /'
  [ "$rc" = 0 ] && ok "grpc interop ($label, exit 0)" || bad "grpc interop ($label, exit $rc)"
  # gRPC-Web（F-209）: ブラウザの代わりに curl。HelloRequest{name:"web"} の枠と、その base64
  local U="http://127.0.0.1:$P/demo.v1.Demo/SayHello"
  expect "grpc-web binary over HTTP/1.1 ($label)" "Hello web" bash -c "printf '\\000\\000\\000\\000\\005\\012\\003web' | curl -s --http1.1 -X POST --data-binary @- -H 'content-type: application/grpc-web+proto' '$U' | LC_ALL=C tr -c '[:print:]' '.'"
  expect "grpc-web trailer frame ($label)" "grpc-status: 0" bash -c "printf '\\000\\000\\000\\000\\005\\012\\003web' | curl -s --http1.1 -X POST --data-binary @- -H 'content-type: application/grpc-web+proto' '$U' | LC_ALL=C tr -c '[:print:]' '.'"
  expect "grpc-web-text (h2c, $label)" "Hello web" bash -c "printf 'AAAAAAUKA3dlYg==' | curl -s --http2-prior-knowledge -X POST --data-binary @- -H 'content-type: application/grpc-web-text' '$U' | $PY3 -c 'import sys,base64; sys.stdout.write(base64.b64decode(sys.stdin.read()).decode(\"latin-1\"))' | LC_ALL=C tr -c '[:print:]' '.'"
  expect "grpc-web CORS preflight ($label)" "access-control-allow-origin: https://app.example" bash -c "curl -s -i -X OPTIONS -H 'origin: https://app.example' -H 'access-control-request-method: POST' '$U' | tr A-Z a-z"
  expect "unknown gRPC service is UNIMPLEMENTED, not 404 ($label)" "grpc-status: 12" bash -c "curl -s -i --http2-prior-knowledge -X POST -H 'content-type: application/grpc' -H 'te: trailers' --data-binary '' 'http://127.0.0.1:$P/nope.v1.Nope/Call' | tr A-Z a-z"
  expect "grpc-web CORS rejects other origins ($label)" "403" curl -s -o /dev/null -w "%{http_code}" -X OPTIONS -H 'origin: https://evil.example' "$U"
  kill "$PID"
}

run_upgrade() { # F-261: ハンドラの HTTP/1.1 アップグレード（examples/api/rust-websocket の x-echo）
  echo "== HTTP/1.1 upgrade (examples/api/rust-websocket, F-261)"
  upgrade_suite "$ROOT/target/examples-api" ""
  # Linux は既定の io_uring に加えて epoll の reactor でも確かめる
  [ "$OS" = Linux ] && upgrade_suite "$ROOT/target/examples-api-epoll" "veil/epoll"
}

upgrade_suite() { # upgrade_suite <target-dir> <extra features>
  local T="$1" F="$2" label="${2:-default}"
  (cd "$ROOT/examples/api/rust-websocket" && CARGO_TARGET_DIR="$T" cargo build -q ${F:+--features "$F"}) || { bad "build websocket example ($label)"; return; }
  local BIN="$T/debug/veil-example-websocket$EXE"
  local W; W="$(mktemp -d)"; [ "$OS" = Windows ] && W="$(cygpath -m "$W")"
  MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*' openssl req -x509 -newkey rsa:2048 -nodes -keyout "$W/key.pem" -out "$W/cert.pem" -days 2 \
    -subj /CN=localhost -addext subjectAltName=DNS:localhost,IP:127.0.0.1 2>/dev/null || { bad "cert"; return; }
  local ktls_modes="0"; [ "$OS" = Linux ] && ktls_modes="0 1"
  local K
  for K in $ktls_modes; do
    local P S; P=$(free_port); S=$(free_port)
    (cd "$W" && VEIL_LISTEN="127.0.0.1:$P" VEIL_TLS_LISTEN="127.0.0.1:$S" VEIL_TLS_CERT="$W/cert.pem" VEIL_TLS_KEY="$W/key.pem" \
      VEIL_KTLS=$K VEIL_SHUTDOWN_SECS=10 exec "$BIN") > "$W/log" 2>&1 &
    local PID=$!; PIDS+=("$PID")
    wait_port "$P" "$PID" && wait_port "$S" "$PID" || { bad "start ($label)"; cat "$W/log"; return; }
    local tag="$label, ktls=$K"
    if [ "$K" = 0 ]; then
      "$PY3" -I "$ROOT/tests/api/upgrade_check.py" basic "$P" | sed "s|\$| ($tag, plaintext)|; s|^|  |" | tee "$W/r"
      grep -q "FAIL" "$W/r" && FAIL=1
      "$PY3" -I "$ROOT/tests/api/upgrade_check.py" many "$P" 1000 | sed "s|\$| ($tag)|; s|^|  |" | tee "$W/r"
      grep -q "^  ok" "$W/r" || FAIL=1
      expect "h2c: upgrade response is 400 ($tag)" "400" curl -s -o /dev/null -w "%{http_code}" --http2-prior-knowledge "http://127.0.0.1:$P/raw-upgrade"
      expect "HTTP/2 (TLS): upgrade response is 400 ($tag)" "400" curl -ks --http2 -o /dev/null -w "%{http_code}" "https://127.0.0.1:$S/raw-upgrade"
    fi
    "$PY3" -I "$ROOT/tests/api/upgrade_check.py" basic "$S" tls | sed "s|\$| ($tag)|; s|^|  |" | tee "$W/r"
    grep -q "FAIL" "$W/r" && FAIL=1
    "$PY3" -I "$ROOT/tests/api/upgrade_check.py" many "$S" 200 tls | sed "s|\$| ($tag)|; s|^|  |" | tee "$W/r"
    grep -q "^  ok" "$W/r" || FAIL=1
    if [ "$OS" != Windows ] && [ "$K" = 0 ]; then
      # graceful shutdown: 開いているアップグレード済みの接続は合図を受けて閉じ、プロセスは待ち時間より前に終わる
      "$PY3" -I "$ROOT/tests/api/upgrade_check.py" hold "$P" 20 > "$W/hold" 2>&1 &
      local HP=$!
      for _ in $(seq 1 600); do grep -q held "$W/hold" && break; sleep 0.1; done
      kill "$PID"; wait "$HP"
      sed "s|\$| ($tag)|; s|^|  |" "$W/hold" | grep -v held
      grep -q "^ok" "$W/hold" || FAIL=1
      for _ in $(seq 1 40); do kill -0 "$PID" 2>/dev/null || break; sleep 0.2; done
      kill -0 "$PID" 2>/dev/null && bad "process exits after closing upgraded connections ($tag)" || ok "process exits after closing upgraded connections ($tag)"
    fi
    kill "$PID" 2>/dev/null; for _ in $(seq 1 50); do kill -0 "$PID" 2>/dev/null || break; sleep 0.2; done
  done
  # max_concurrent_connections にアップグレードした接続も数える
  if [ "$OS" != Windows ]; then
    local P; P=$(free_port)
    (cd "$W" && VEIL_LISTEN="127.0.0.1:$P" VEIL_MAX_CONN=5 VEIL_SHUTDOWN_SECS=1 exec "$BIN") > "$W/log2" 2>&1 &
    local PID=$!; PIDS+=("$PID")
    wait_port "$P" "$PID" || { bad "start max_conn ($label)"; cat "$W/log2"; return; }
    sleep 0.5  # wait_port の接続が閉じて数から外れるまで
    "$PY3" -I "$ROOT/tests/api/upgrade_check.py" hold "$P" 5 > "$W/hold" 2>&1 &
    local HP=$!
    for _ in $(seq 1 600); do grep -q held "$W/hold" && break; sleep 0.1; done
    grep -q held "$W/hold" || { bad "open 5 upgraded connections ($label)"; cat "$W/hold"; }
    "$PY3" -I "$ROOT/tests/api/upgrade_check.py" refused "$P" | sed "s|\$| ($label)|; s|^|  |" | tee "$W/r"
    grep -q "^  ok" "$W/r" || FAIL=1
    kill "$PID"; wait "$HP" 2>/dev/null
  fi
}

run_external() { # F-243: [env] 無しの利用者のビルド（と、[env] を書いた利用者のビルド）
  echo "== external project without / with .cargo/config.toml [env] (F-243)"
  [ "$OS" = Linux ] || { echo "  skip: Linux only"; return; }
  local SRC="${VEIL_EXTERNAL_SRC:-$ROOT}" H3IMG="${H3_CURL_IMAGE:-alpine/curl-http3:latest}"
  # cargo は作業ディレクトリから上へ .cargo/config.toml を探すので、リポジトリの外（$HOME の下）に作る
  local BASE; BASE="$(mktemp -d "${VEIL_EXTERNAL_DIR:-$HOME}/veil-external.XXXXXX")"
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 2 -subj /CN=localhost \
    -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" -keyout "$BASE/key.pem" -out "$BASE/cert.pem" 2>/dev/null
  local mode
  for mode in noenv env; do
    local W="$BASE/$mode"; mkdir -p "$W/src"
    cat > "$W/Cargo.toml" <<TOML
[package]
name = "veil-external-$mode"
version = "0.1.0"
edition = "2021"

[dependencies]
veil = { package = "veil-proxy", path = "$SRC", default-features = false, features = ["full", "api"] }

[workspace]
TOML
    if [ "$mode" = env ]; then
      mkdir -p "$W/.cargo"
      printf '[env]\nAWS_LC_SYS_NO_PREFIX_%s = { value = "1", force = true }\n' "$(rustc -vV | sed -n 's/^host: //p' | tr - _)" > "$W/.cargo/config.toml"
    fi
    cat > "$W/src/main.rs" <<'RS'
use veil::prelude::*;

#[veil::route(path = "/hello", method = "GET")]
async fn hello() -> &'static str {
    "hello from an external crate"
}

#[veil::main]
async fn main() -> Result<(), veil::api::Error> {
    let e = |k: &str| std::env::var(k).unwrap();
    let cfg = Config::new()
        .builder()
        .listen_cleartext(&e("PLAIN"))
        .listen(&e("TLS"))
        .tls(&e("CERT"), &e("KEY"))
        .http3(&e("TLS"))
        .build();
    veil::serve(cfg).await
}
RS
    if ! (cd "$W" && env -u AWS_LC_SYS_NO_PREFIX cargo build -q) > "$W/build.log" 2>&1; then
      bad "$mode: build"; tail -30 "$W/build.log"; continue
    fi
    ok "$mode: build"
    local bin="$W/target/debug/veil-external-$mode"
    # quiche の FFI が、[env] 無しは接頭辞付き（aws_lc_<版>_EVP_AEAD_CTX_seal_scatter）、有りは接頭辞無しの aws-lc に
    # リンクされている（aws-lc-sys が接頭辞を付けるのは libcrypto だけで、libssl の SSL_* はどちらでも接頭辞無し）
    local sym=EVP_AEAD_CTX_seal_scatter
    nm "$bin" > "$W/nm.txt"   # pipefail の下で `nm | grep -q` は grep が先に終わると SIGPIPE で失敗扱いになる
    if [ "$mode" = noenv ]; then
      grep -Eq " [Tt] aws_lc_[0-9_]+_$sym\$" "$W/nm.txt" && ! grep -Eq " [Tt] $sym\$" "$W/nm.txt" \
        && ok "$mode: quiche links the prefixed aws-lc symbols" || bad "$mode: unexpected $sym symbols: $(grep -E "$sym\$" "$W/nm.txt" | head -3 | tr '\n' ' ')"
    else
      grep -Eq " [Tt] $sym\$" "$W/nm.txt" && ok "$mode: quiche links the unprefixed aws-lc symbols" || bad "$mode: no unprefixed $sym"
    fi
    # cargo tree -i は見つからなくても 0 で終わり何も出さない
    [ -z "$(cd "$W" && cargo tree -q -i boring 2>/dev/null)" ] && ok "$mode: no boring crate" || bad "$mode: depends on boring"
    local n; n=$(cd "$W" && cargo tree -q -i aws-lc-sys --depth 0 2>/dev/null | grep -c '^aws-lc-sys')
    [ "$n" = 1 ] && ok "$mode: one aws-lc-sys" || bad "$mode: $n aws-lc-sys"
    local P T; P=$(free_port); T=$(free_port)
    PLAIN="127.0.0.1:$P" TLS="127.0.0.1:$T" CERT="$BASE/cert.pem" KEY="$BASE/key.pem" "$bin" > "$W/run.log" 2>&1 &
    local PID=$!; PIDS+=("$PID")
    wait_port "$P" "$PID" || { bad "$mode: start"; tail -20 "$W/run.log"; continue; }
    expect "$mode: HTTP/1.1" "hello from an external crate" curl -s "http://127.0.0.1:$P/hello"
    expect "$mode: HTTP/2 (TLS)" "2" curl -ks --http2 -o /dev/null -w '%{http_version}' "https://127.0.0.1:$T/hello"
    expect "$mode: HTTP/3" "3 hello from an external crate" docker run --rm --network host --entrypoint sh "$H3IMG" -c \
      "curl -ks --http3-only -w '%{http_version} ' -o /tmp/b https://127.0.0.1:$T/hello && cat /tmp/b"
    kill "$PID"
  done
  rm -rf "$BASE"
}

run_websocket() { # F-262: ハンドラの WebSocket（examples/api/rust-websocket の /ws・/chat・/typed）と Autobahn TestSuite
  echo "== WebSocket (examples/api/rust-websocket, F-262)"
  websocket_suite "$ROOT/target/examples-api" ""
  [ "$OS" = Linux ] && websocket_suite "$ROOT/target/examples-api-epoll" "veil/epoll"
  run_autobahn
}

websocket_suite() { # websocket_suite <target-dir> <extra features>
  local T="$1" F="$2" label="${2:-default}"
  (cd "$ROOT/examples/api/rust-websocket" && CARGO_TARGET_DIR="$T" cargo build -q ${F:+--features "$F"}) || { bad "build websocket example ($label)"; return; }
  local BIN="$T/debug/veil-example-websocket$EXE"
  local W; W="$(mktemp -d)"; [ "$OS" = Windows ] && W="$(cygpath -m "$W")"
  MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*' openssl req -x509 -newkey rsa:2048 -nodes -keyout "$W/key.pem" -out "$W/cert.pem" -days 2 \
    -subj /CN=localhost -addext subjectAltName=DNS:localhost,IP:127.0.0.1 2>/dev/null || { bad "cert"; return; }
  # 同時接続の数（既定 2,000。WS_E2E_CONNS=16384 で HttpArena の echo-ws の最大。ulimit -n を上げてから）
  local CONNS="${WS_E2E_CONNS:-2000}"
  local ktls_modes="0"; [ "$OS" = Linux ] && ktls_modes="0 1"
  local K
  for K in $ktls_modes; do
    local P S; P=$(free_port); S=$(free_port)
    (cd "$W" && VEIL_LISTEN="127.0.0.1:$P" VEIL_TLS_LISTEN="127.0.0.1:$S" VEIL_TLS_CERT="$W/cert.pem" VEIL_TLS_KEY="$W/key.pem" \
      VEIL_KTLS=$K VEIL_SHUTDOWN_SECS=10 exec "$BIN") > "$W/log" 2>&1 &
    local PID=$!; PIDS+=("$PID")
    wait_port "$P" "$PID" && wait_port "$S" "$PID" || { bad "start ($label)"; cat "$W/log"; return; }
    local tag="$label, ktls=$K"
    if [ "$K" = 0 ]; then
      "$PY3" -I "$ROOT/tests/api/ws_check.py" rust "$P" | sed "s|\$| ($tag)|; s|^|  |" | tee "$W/r"
      grep -q "FAIL" "$W/r" && FAIL=1
      "$PY3" -I "$ROOT/tests/api/ws_check.py" many "$P" "$CONNS" | sed "s|\$| ($tag)|; s|^|  |" | tee "$W/r"
      grep -q "^  ok" "$W/r" || FAIL=1
      "$PY3" -I "$ROOT/tests/api/ws_check.py" churn "$P" 200 | sed "s|\$| ($tag)|; s|^|  |" | tee "$W/r"
      grep -q "^  ok" "$W/r" || FAIL=1
      expect "OpenAPI lists /ws with x-veil-websocket ($tag)" "echo.v1" bash -c "curl -s 'http://127.0.0.1:$P/openapi.json' | $PY3 -I -c 'import json,sys; o=json.load(sys.stdin)[\"paths\"][\"/ws\"][\"get\"]; print(\"101\" in o[\"responses\"], o[\"x-veil-websocket\"][\"subprotocols\"])'"
      expect "OpenAPI /typed has receive/send schemas ($tag)" "ChatIn ChatOut" bash -c "curl -s 'http://127.0.0.1:$P/openapi.json' | $PY3 -I -c 'import json,sys; x=json.load(sys.stdin)[\"paths\"][\"/typed\"][\"get\"][\"x-veil-websocket\"]; print(x[\"receive\"][\"\$ref\"].split(\"/\")[-1], x[\"send\"][\"\$ref\"].split(\"/\")[-1])'"
      expect "WebSocket on h2c is 400 ($tag)" "400" curl -s -o /dev/null -w "%{http_code}" --http2-prior-knowledge "http://127.0.0.1:$P/ws"
    fi
    "$PY3" -I "$ROOT/tests/api/ws_check.py" rust "$S" tls | sed "s|\$| ($tag)|; s|^|  |" | tee "$W/r"
    grep -q "FAIL" "$W/r" && FAIL=1
    "$PY3" -I "$ROOT/tests/api/ws_check.py" many "$S" 500 tls | sed "s|\$| ($tag)|; s|^|  |" | tee "$W/r"
    grep -q "^  ok" "$W/r" || FAIL=1
    if [ "$OS" != Windows ] && [ "$K" = 0 ]; then
      # graceful shutdown: 開いている WebSocket に Close 1001 を送ってから閉じる
      "$PY3" -I "$ROOT/tests/api/ws_check.py" hold "$P" 20 > "$W/hold" 2>&1 &
      local HP=$!
      for _ in $(seq 1 600); do grep -q held "$W/hold" && break; sleep 0.1; done
      kill "$PID"; wait "$HP"
      grep -q "close 1001" "$W/hold" && ok "graceful shutdown sends Close 1001 ($tag)" || bad "graceful shutdown sends Close 1001 ($tag): $(cat "$W/hold")"
      for _ in $(seq 1 40); do kill -0 "$PID" 2>/dev/null || break; sleep 0.2; done
      kill -0 "$PID" 2>/dev/null && bad "process exits after closing WebSockets ($tag)" || ok "process exits after closing WebSockets ($tag)"
    fi
    kill "$PID" 2>/dev/null; for _ in $(seq 1 50); do kill -0 "$PID" 2>/dev/null || break; sleep 0.2; done
  done
}

run_autobahn() { # Autobahn TestSuite の fuzzingclient（docker と crossbario/autobahn-testsuite が要る。無ければ飛ばす）
  if ! command -v docker >/dev/null || ! docker image inspect crossbario/autobahn-testsuite >/dev/null 2>&1; then
    echo "  skip Autobahn TestSuite (docker or crossbario/autobahn-testsuite is missing; docker pull crossbario/autobahn-testsuite)"
    return
  fi
  local T="$ROOT/target/examples-api"
  (cd "$ROOT/examples/api/rust-websocket" && CARGO_TARGET_DIR="$T" cargo build -q --release) || { bad "build websocket example (release)"; return; }
  local BIN="$T/release/veil-example-websocket$EXE"
  # snap の docker は /tmp を見られないので作業場所は target/ の下に置く
  mkdir -p "$ROOT/target"
  local W; W="$(mktemp -d "$ROOT/target/autobahn.XXXXXX")"
  openssl req -x509 -newkey rsa:2048 -nodes -keyout "$W/key.pem" -out "$W/cert.pem" -days 2 \
    -subj /CN=localhost -addext subjectAltName=DNS:localhost,IP:127.0.0.1 2>/dev/null || { bad "cert"; return; }
  # 1.*〜7.*・9.*（性能）・10.* は圧縮無しで、12.*・13.*（permessage-deflate）は websocket_compression = true で回す。
  # TLS は 1.*〜7.* だけ（同じ復号器なので時間を抑える）
  local P S; P=$(free_port); S=$(free_port)
  (cd "$W" && VEIL_LISTEN="127.0.0.1:$P" VEIL_TLS_LISTEN="127.0.0.1:$S" VEIL_TLS_CERT="$W/cert.pem" VEIL_TLS_KEY="$W/key.pem" \
    VEIL_WS_COMPRESSION=1 exec "$BIN") > "$W/log" 2>&1 &
  local PID=$!; PIDS+=("$PID")
  wait_port "$P" "$PID" && wait_port "$S" "$PID" || { bad "start (autobahn)"; cat "$W/log"; return; }
  cat > "$W/fuzzingclient.json" <<JSON
{ "outdir": "/w/reports", "servers": [
    {"agent": "veil", "url": "ws://127.0.0.1:$P/ws"},
    {"agent": "veil-tls", "url": "wss://127.0.0.1:$S/ws"} ],
  "cases": ["*"], "exclude-cases": [], "exclude-agent-cases": {"veil-tls": ["9.*", "10.*", "12.*", "13.*"]} }
JSON
  tmo 3600 docker run --rm --network host --user "$(id -u):$(id -g)" -v "$W:/w" crossbario/autobahn-testsuite \
    wstest -m fuzzingclient -s /w/fuzzingclient.json > "$W/autobahn.log" 2>&1 || { bad "Autobahn fuzzingclient run"; tail -20 "$W/autobahn.log"; }
  autobahn_summary "$W/reports/index.json" | tee "$W/r"
  grep -q "^  ok" "$W/r" && ! grep -q "FAIL" "$W/r" || FAIL=1
  [ -n "${AUTOBAHN_REPORTS:-}" ] && cp -r "$W/reports" "$AUTOBAHN_REPORTS"
  kill "$PID" 2>/dev/null
  rm -rf "$W"
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
  # F-261: アップグレードの記号（veil::api::upgrade）も入らない
  n=$(grep -cE "veil::api::upgrade|veil_response_upgrade" "$SYMS") || true
  [ "$n" = 0 ] && ok "no veil::api::upgrade in the full binary" || bad "upgrade symbols found ($n)"
  # F-262: WebSocket の終端（veil::api::ws・C の veil_ws_*）も入らない
  n=$(grep -cE "veil::api::ws|veil_ws_" "$SYMS") || true
  [ "$n" = 0 ] && ok "no veil::api::ws in the full binary" || bad "WebSocket handler symbols found ($n)"
  # F-266: multipart（veil::api::multipart・C の veil_multipart_next）も入らない
  n=$(grep -cE "veil::api::multipart|veil_multipart_|multipart_parse" "$SYMS") || true
  [ "$n" = 0 ] && ok "no veil::api::multipart in the full binary" || bad "multipart symbols found ($n)"
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
  upgrade) run_upgrade ;;
  websocket) run_websocket ;;
  autobahn) run_autobahn ;;
  c) run_c; run_c_features ;;
  python) run_python ;;
  nonregression) run_nonregression ;;
  external) run_external ;;
  all) run_nonregression; run_rust; run_graphql; run_grpc; run_upgrade; run_websocket; run_c; run_c_features; run_python ;;
  *) echo "usage: $0 [rust|grpc|upgrade|websocket|autobahn|c|python|nonregression|external|all]"; exit 2 ;;
esac
[ "$FAIL" = 0 ] && echo "ALL OK" || { echo "SOME CHECKS FAILED"; exit 1; }
