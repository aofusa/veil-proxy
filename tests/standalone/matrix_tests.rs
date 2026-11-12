//! 機能 × プロトコルのマトリックス（feat/beat-nginx の設計変更の確認。F-303〜F-305・B-360・B-361）。
//!
//! 1 つの veil に機能ごとのルートを並べ、HTTP/1.1（平文・TLS）・h2c・HTTP/2（TLS）・HTTP/3 の全部で同じ要求を
//! 送って、機能ごとの期待を確かめる。失敗は全部集めて表にしてから落とす（どの組み合わせが壊れたかが一度に分かる）。
//! `cargo test --test standalone_e2e --features full matrix -- --nocapture` で表を出す。

use super::standalone::parity::*;
use super::standalone::*;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// マトリックスで試すプロトコル（parity の 4 つに TLS の HTTP/1.1 を足す）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MProto {
    H1,
    H1Tls,
    H2c,
    H2Tls,
    H3,
}

impl MProto {
    fn all() -> Vec<MProto> {
        let mut v = vec![MProto::H1, MProto::H1Tls, MProto::H2c, MProto::H2Tls];
        if cfg!(feature = "http3") {
            v.push(MProto::H3);
        }
        v
    }

    fn name(self) -> &'static str {
        match self {
            MProto::H1 => "h1",
            MProto::H1Tls => "h1tls",
            MProto::H2c => "h2c",
            MProto::H2Tls => "h2tls",
            MProto::H3 => "h3",
        }
    }

    fn is_h1(self) -> bool {
        matches!(self, MProto::H1 | MProto::H1Tls)
    }
}

/// `proto` で 1 要求を送る。送受信の失敗は `Err`
fn mfetch(
    proto: MProto,
    ports: Ports,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<HttpResponse, String> {
    let p = match proto {
        MProto::H1Tls => return h1_tls_fetch(ports.tls, method, path, headers, body),
        MProto::H1 => Proto::H1,
        MProto::H2c => Proto::H2c,
        MProto::H2Tls => Proto::H2Tls,
        MProto::H3 => Proto::H3,
    };
    // parity の fetch は送受信の失敗で panic するので、ここで Err に変える
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        fetch(p, ports, method, path, headers, body)
    }))
    .map_err(|e| {
        e.downcast_ref::<String>()
            .cloned()
            .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "panic".to_string())
    })
}

/// TLS の HTTP/1.1（ALPN http/1.1、`Connection: close`）で 1 要求を送る
fn h1_tls_fetch(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<HttpResponse, String> {
    let cfg = Arc::new(tls_client_config(b"http/1.1"));
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let conn = rustls::ClientConnection::new(cfg, name).map_err(|e| e.to_string())?;
    let tcp = connect_local(port, Duration::from_secs(2)).map_err(|e| e.to_string())?;
    tcp.set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    let mut tls = rustls::StreamOwned::new(conn, tcp);
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    if !body.is_empty() {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    tls.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    tls.write_all(body).map_err(|e| e.to_string())?;
    tls.flush().map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    let mut buf = [0u8; 16384];
    loop {
        match tls.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
            // close_notify 無しの切断も、読めた分で解析する
            Err(_) => break,
        }
    }
    let mut r = parse_http1_response(&raw).map_err(|e| e.to_string())?;
    if method == "HEAD" || r.status == 304 {
        let end = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        r.body = raw[end..].to_vec();
    }
    Ok(r)
}

fn text_body(len: usize) -> Vec<u8> {
    (0..len).map(|i| b'a' + (i % 26) as u8).collect()
}

fn binary_body(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
}

fn gunzip(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(data)
        .read_to_end(&mut out)
        .map_err(|e| format!("gzip: {e}"))?;
    Ok(out)
}

fn unbrotli(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    brotli::Decompressor::new(data, 4096)
        .read_to_end(&mut out)
        .map_err(|e| format!("br: {e}"))?;
    Ok(out)
}

/// 期待の確認を `Err` に変える
macro_rules! check {
    ($cond:expr, $($arg:tt)+) => {
        if !$cond {
            return Err(format!($($arg)+));
        }
    };
}

fn status_is(r: &HttpResponse, want: u16) -> Result<(), String> {
    check!(
        r.status == want,
        "status {} (want {want}) headers {:?} body {:?}",
        r.status,
        r.headers,
        String::from_utf8_lossy(&r.body[..r.body.len().min(200)])
    );
    Ok(())
}

/// HTTP/2・HTTP/3 の応答に接続固有のヘッダが無い（RFC 9113 §8.2.2・RFC 9114 §4.2）。
/// HTTP/1.1 は `Connection: close` の要求に `close` で答える（B-360）
fn connection_headers_ok(proto: MProto, r: &HttpResponse) -> Result<(), String> {
    if proto.is_h1() {
        check!(
            r.header("connection")
                .is_some_and(|v| v.eq_ignore_ascii_case("close")),
            "HTTP/1.1 answer to Connection: close lacks `Connection: close`: {:?}",
            r.headers
        );
        check!(
            r.header("keep-alive").is_none(),
            "Keep-Alive on a closing answer: {:?}",
            r.headers
        );
    } else {
        for h in [
            "connection",
            "keep-alive",
            "transfer-encoding",
            "upgrade",
            "proxy-connection",
        ] {
            check!(
                r.header(h).is_none(),
                "connection-specific header `{h}` on {proto:?}: {:?}",
                r.headers
            );
        }
    }
    Ok(())
}

type Seen = Arc<Mutex<Vec<UpstreamSeen>>>;

fn seen_count(seen: &Seen, needle: &str) -> usize {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|s| s.head.lines().next().is_some_and(|l| l.contains(needle)))
        .count()
}

/// Prometheus の `veil_proxy_http_requests_total` の全系列の合計
fn requests_total(ports: Ports) -> Result<f64, String> {
    let m = http1_get(ports.clear, "/__metrics")
        .map_err(|e| e.to_string())?
        .text();
    Ok(m.lines()
        .filter(|l| l.starts_with("veil_proxy_http_requests_total{"))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<f64>().ok())
        .sum())
}

/// 応答ヘッダに `x-wasm-resp: 1` を足す Proxy-Wasm モジュール（WAT）
const RESPONSE_HEADER_ADDER: &str = r#"
(module
  (import "env" "proxy_add_header_map_value"
    (func $add (param i32 i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2)
  (data (i32.const 16) "x-wasm-resp")
  (data (i32.const 32) "1")
  (global $bump (mut i32) (i32.const 65536))
  (func (export "proxy_on_memory_allocate") (param $size i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get $size)))
    (local.get $p))
  (func (export "proxy_on_response_headers") (param $ctx i32) (param $n i32) (param $eos i32) (result i32)
    (drop (call $add (i32.const 2) (i32.const 16) (i32.const 11) (i32.const 32) (i32.const 1)))
    (i32.const 0))
)
"#;

/// 機能 × プロトコルの全組み合わせを 1 つの veil で確かめる
#[test]
fn feature_protocol_matrix() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    let www = dir.path().join("www");
    std::fs::create_dir_all(&www).unwrap();
    let small = text_body(5000);
    let big = binary_body(300_000);
    std::fs::write(www.join("a.txt"), &small).unwrap();
    std::fs::write(www.join("big.bin"), &big).unwrap();
    let wat = dir.path().join("adder.wat");
    std::fs::write(&wat, RESPONSE_HEADER_ADDER).unwrap();
    let access_log = dir.path().join("access.log");

    let up_big = binary_body(400_000);
    let up_big2 = up_big.clone();
    let (up, seen) = spawn_recording_upstream(move |head| {
        let line = head.lines().next().unwrap_or("");
        if line.contains("/big") {
            (
                200,
                "Content-Type: application/octet-stream\r\n".to_string(),
                up_big2.clone(),
            )
        } else if line.contains("/echo") {
            (
                200,
                "Content-Type: text/plain\r\n".to_string(),
                b"ok".to_vec(),
            )
        } else {
            (
                200,
                "Content-Type: text/plain\r\nKeep-Alive: timeout=5\r\nConnection: keep-alive\r\n"
                    .to_string(),
                vec![b'u'; 2000],
            )
        }
    });
    let (lb_a, seen_a) = spawn_recording_upstream(|_| (200, String::new(), b"A".to_vec()));
    let (lb_b, seen_b) = spawn_recording_upstream(|_| (200, String::new(), b"B".to_vec()));
    let (h2up, h2seen) = spawn_recording_h2c_upstream();
    let ports = Ports::alloc();
    let protos = MProto::all();

    let gz =
        "[route.compression]\nenabled = true\npreferred_encodings = [\"gzip\"]\nmin_size = 100\n";
    let br =
        "[route.compression]\nenabled = true\npreferred_encodings = [\"br\"]\nmin_size = 100\n";
    let mut per_proto_routes = String::new();
    for p in &protos {
        per_proto_routes.push_str(&format!(
            r#"
[[route]]
[route.conditions]
path = "/rl-{p}"
[route.action]
type = "Respond"
status = 200
body = "rl"
[route.security]
rate_limit_requests_per_min = 3
"#,
            p = p.name()
        ));
    }
    let config = format!(
        r#"{listeners}
[prometheus]
enabled = true

[access_log]
enabled = true
format = "json"
file_path = "{access_log}"
flush_interval_ms = 100

[wasm]
enabled = true
[[wasm.modules]]
name = "adder"
path = "{wat}"
[wasm.modules.capabilities]
allow_response_headers_read = true
allow_response_headers_write = true

[upstreams."lb"]
algorithm = "round_robin"
servers = ["http://127.0.0.1:{lb_a}", "http://127.0.0.1:{lb_b}"]

[[route]]
[route.conditions]
path = "/file/*"
[route.action]
type = "File"
path = "{www}"
mode = "sendfile"

[[route]]
[route.conditions]
path = "/mem"
[route.action]
type = "File"
path = "{www}/a.txt"
mode = "memory"

[[route]]
[route.conditions]
path = "/cached/*"
[route.action]
type = "File"
path = "{www}"
mode = "sendfile"
[route.open_file_cache]
enabled = true
[route.static_file_cache]
enabled = true

[[route]]
[route.conditions]
path = "/gz/*"
[route.action]
type = "File"
path = "{www}"
mode = "sendfile"
{gz}
[[route]]
[route.conditions]
path = "/br/*"
[route.action]
type = "File"
path = "{www}"
mode = "sendfile"
{br}
[[route]]
[route.conditions]
path = "/proxy/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up}/"

[[route]]
[route.conditions]
path = "/h2up/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{h2up}/"
use_h2c = true

[[route]]
[route.conditions]
path = "/pbuf/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up}/"
[route.buffering]
mode = "full"

[[route]]
[route.conditions]
path = "/pgz/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up}/"
{gz}
[[route]]
[route.conditions]
path = "/cache/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up}/"
[route.cache]
enabled = true
default_ttl_secs = 300

[[route]]
[route.conditions]
path = "/lb/*"
[route.action]
type = "Proxy"
upstream = "lb"

[[route]]
modules = ["adder"]
[route.conditions]
path = "/wasm/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up}/"

[[route]]
[route.conditions]
path = "/small-body/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up}/"
[route.security]
max_request_body_size = 1024

[[route]]
[route.conditions]
path = "/hdr"
[route.action]
type = "Respond"
status = 200
body = "hdr"
[route.security]
add_response_headers = {{ "X-Added" = "1" }}
remove_response_headers = ["Server"]

[[route]]
[route.conditions]
path = "/respond"
[route.action]
type = "Respond"
status = 200
body = "respond-body"

[[route]]
[route.conditions]
path = "/al/*"
[route.action]
type = "Respond"
status = 200
body = "logged"

[[route]]
[route.conditions]
path = "/redirect"
[route.action]
type = "Redirect"
redirect_url = "https://example.com/new"
redirect_status = 301

[[route]]
[route.conditions]
path = "/getonly"
[route.action]
type = "Respond"
status = 200
body = "get"
[route.security]
allowed_methods = ["GET", "HEAD"]

[[route]]
[route.conditions]
path = "/cors"
[route.action]
type = "Respond"
status = 200
body = "cors"
[route.traffic.cors]
allow_origins = ["https://app.example.com"]
allow_methods = ["GET", "POST"]
allow_headers = ["content-type"]
max_age_secs = 600

[[route]]
[route.conditions]
path = "/fault"
[route.action]
type = "Respond"
status = 200
body = "never"
[route.traffic.fault]
abort_status = 503
abort_percent = 100.0
{per_proto_routes}"#,
        listeners = listeners_toml(ports, &cert, &key),
        access_log = tp(&access_log),
        wat = tp(&wat),
        www = tp(&www),
    );
    let veil = spawn_config(dir.path(), ports, &config);

    type Case<'a> = (&'static str, Box<dyn Fn(MProto) -> Result<(), String> + 'a>);
    let get = |p: MProto, path: &str, h: &[(&str, &str)]| mfetch(p, ports, "GET", path, h, b"");
    let cases: Vec<Case> = vec![
        (
            "static sendfile (GET/HEAD/Range)",
            Box::new(|p| {
                let r = get(p, "/file/a.txt", &[])?;
                status_is(&r, 200)?;
                check!(r.body == small, "body differs ({} bytes)", r.body.len());
                let h = mfetch(p, ports, "HEAD", "/file/a.txt", &[], b"")?;
                status_is(&h, 200)?;
                check!(h.body.is_empty(), "HEAD carried {} bytes", h.body.len());
                let rg = get(p, "/file/a.txt", &[("range", "bytes=10-19")])?;
                status_is(&rg, 206)?;
                check!(rg.body == small[10..20], "range body {:?}", rg.text());
                Ok(())
            }),
        ),
        (
            "static large file (300 KB)",
            Box::new(|p| {
                let r = get(p, "/file/big.bin", &[])?;
                status_is(&r, 200)?;
                check!(r.body == big, "body differs ({} bytes)", r.body.len());
                Ok(())
            }),
        ),
        (
            "static memory",
            Box::new(|p| {
                let r = get(p, "/mem", &[])?;
                status_is(&r, 200)?;
                check!(r.body == small, "body differs ({} bytes)", r.body.len());
                Ok(())
            }),
        ),
        (
            "static cached (open_file_cache + static_file_cache)",
            Box::new(|p| {
                for _ in 0..3 {
                    let r = get(p, "/cached/a.txt", &[])?;
                    status_is(&r, 200)?;
                    check!(r.body == small, "body differs ({} bytes)", r.body.len());
                    let r = get(p, "/cached/big.bin", &[])?;
                    status_is(&r, 200)?;
                    check!(r.body == big, "big body differs ({} bytes)", r.body.len());
                }
                Ok(())
            }),
        ),
        (
            "static conditional (304)",
            Box::new(|p| {
                let r = get(p, "/file/a.txt", &[])?;
                let etag = r.header("etag").ok_or("no etag")?.to_string();
                let c = get(p, "/file/a.txt", &[("if-none-match", etag.as_str())])?;
                status_is(&c, 304)?;
                check!(c.body.is_empty(), "304 carried {} bytes", c.body.len());
                Ok(())
            }),
        ),
        (
            "compression gzip (static)",
            Box::new(|p| {
                let r = get(p, "/gz/a.txt", &[("accept-encoding", "gzip")])?;
                status_is(&r, 200)?;
                check!(
                    r.header("content-encoding") == Some("gzip"),
                    "content-encoding {:?}",
                    r.header("content-encoding")
                );
                check!(gunzip(&r.body)? == small, "decoded body differs");
                Ok(())
            }),
        ),
        (
            "compression br (static)",
            Box::new(|p| {
                let r = get(p, "/br/a.txt", &[("accept-encoding", "br")])?;
                status_is(&r, 200)?;
                check!(
                    r.header("content-encoding") == Some("br"),
                    "content-encoding {:?}",
                    r.header("content-encoding")
                );
                check!(unbrotli(&r.body)? == small, "decoded body differs");
                Ok(())
            }),
        ),
        (
            "proxy HTTP/1.1 upstream",
            Box::new(|p| {
                let path = format!("/proxy/plain-{}", p.name());
                let r = get(p, &path, &[])?;
                status_is(&r, 200)?;
                check!(
                    r.body == vec![b'u'; 2000],
                    "body differs ({} bytes)",
                    r.body.len()
                );
                connection_headers_ok(p, &r)?;
                check!(
                    seen_count(&seen, &format!("/plain-{} ", p.name())) == 1,
                    "upstream did not see the request once"
                );
                Ok(())
            }),
        ),
        (
            "proxy large response (400 KB)",
            Box::new(|p| {
                let r = get(p, &format!("/proxy/big-{}", p.name()), &[])?;
                status_is(&r, 200)?;
                check!(r.body == up_big, "body differs ({} bytes)", r.body.len());
                connection_headers_ok(p, &r)?;
                Ok(())
            }),
        ),
        (
            "proxy request body (64 KB POST)",
            Box::new(|p| {
                let body = binary_body(65536);
                let path = format!("/proxy/echo-{}", p.name());
                let r = mfetch(p, ports, "POST", &path, &[], &body)?;
                status_is(&r, 200)?;
                let s = seen.lock().unwrap();
                let up = s
                    .iter()
                    .find(|s| s.head.contains(&format!("/echo-{} ", p.name())))
                    .ok_or("upstream did not see the POST")?;
                check!(up.complete, "upstream body incomplete");
                check!(
                    up.body == body,
                    "upstream body differs ({} bytes)",
                    up.body.len()
                );
                Ok(())
            }),
        ),
        (
            "proxy h2c upstream",
            Box::new(|p| {
                let before = h2seen.lock().unwrap().len();
                let r = get(p, &format!("/h2up/x-{}", p.name()), &[])?;
                status_is(&r, 200)?;
                check!(
                    h2seen.lock().unwrap().len() == before + 1,
                    "h2c upstream did not see the request"
                );
                Ok(())
            }),
        ),
        (
            "proxy buffering = full",
            Box::new(|p| {
                let r = get(p, "/pbuf/x", &[])?;
                status_is(&r, 200)?;
                check!(
                    r.body == vec![b'u'; 2000],
                    "body differs ({} bytes)",
                    r.body.len()
                );
                connection_headers_ok(p, &r)?;
                Ok(())
            }),
        ),
        (
            "proxy + compression gzip",
            Box::new(|p| {
                let r = get(p, "/pgz/x", &[("accept-encoding", "gzip")])?;
                status_is(&r, 200)?;
                check!(
                    r.header("content-encoding") == Some("gzip"),
                    "content-encoding {:?}",
                    r.header("content-encoding")
                );
                check!(gunzip(&r.body)? == vec![b'u'; 2000], "decoded body differs");
                connection_headers_ok(p, &r)?;
                Ok(())
            }),
        ),
        (
            "proxy cache (second GET not forwarded)",
            Box::new(|p| {
                let path = format!("/cache/c-{}", p.name());
                for i in 0..2 {
                    let r = get(p, &path, &[])?;
                    status_is(&r, 200)?;
                    check!(r.body == vec![b'u'; 2000], "body differs on GET {i}");
                }
                let n = seen_count(&seen, &format!("/c-{} ", p.name()));
                // HTTP/2・HTTP/3 のクライアントの要求はキャッシュを見ない（文書にある制限。F-257）。
                // 応答が正しく中継されることだけを確かめ、表では n/a にする
                let want = if p.is_h1() { 1 } else { 2 };
                check!(n == want, "upstream saw {n} requests (want {want})");
                Ok(())
            }),
        ),
        (
            "load balancing (round robin)",
            Box::new(|p| {
                let a0 = seen_a.lock().unwrap().len();
                let b0 = seen_b.lock().unwrap().len();
                let mut bodies = Vec::new();
                for _ in 0..4 {
                    let r = get(p, "/lb/x", &[])?;
                    status_is(&r, 200)?;
                    bodies.push(r.text());
                }
                let a = seen_a.lock().unwrap().len() - a0;
                let b = seen_b.lock().unwrap().len() - b0;
                check!(a >= 1 && b >= 1, "a={a} b={b} bodies={bodies:?}");
                Ok(())
            }),
        ),
        (
            "Proxy-Wasm response headers",
            Box::new(|p| {
                let r = get(p, "/wasm/x", &[])?;
                status_is(&r, 200)?;
                check!(
                    r.header("x-wasm-resp") == Some("1"),
                    "x-wasm-resp {:?}",
                    r.headers
                );
                Ok(())
            }),
        ),
        (
            "request body limit (413)",
            Box::new(|p| {
                let r = mfetch(p, ports, "POST", "/small-body/x", &[], &[b'z'; 4096])?;
                status_is(&r, 413)?;
                Ok(())
            }),
        ),
        (
            "allowed_methods (405)",
            Box::new(|p| {
                let r = mfetch(p, ports, "POST", "/getonly", &[], b"x")?;
                status_is(&r, 405)?;
                let g = get(p, "/getonly", &[])?;
                status_is(&g, 200)?;
                Ok(())
            }),
        ),
        (
            "response header policy",
            Box::new(|p| {
                let r = get(p, "/hdr", &[])?;
                status_is(&r, 200)?;
                check!(r.header("x-added") == Some("1"), "x-added {:?}", r.headers);
                check!(
                    r.header("server").is_none(),
                    "server {:?}",
                    r.header("server")
                );
                Ok(())
            }),
        ),
        (
            "fixed response",
            Box::new(|p| {
                let r = get(p, "/respond", &[])?;
                status_is(&r, 200)?;
                check!(r.text() == "respond-body", "body {:?}", r.text());
                Ok(())
            }),
        ),
        (
            "redirect (301)",
            Box::new(|p| {
                let r = get(p, "/redirect", &[])?;
                status_is(&r, 301)?;
                check!(
                    r.header("location") == Some("https://example.com/new"),
                    "location {:?}",
                    r.header("location")
                );
                Ok(())
            }),
        ),
        (
            "rate limit (429)",
            Box::new(|p| {
                let path = format!("/rl-{}", p.name());
                let mut st = Vec::new();
                for _ in 0..4 {
                    st.push(get(p, &path, &[])?.status);
                }
                check!(st == [200, 200, 200, 429], "statuses {st:?}");
                Ok(())
            }),
        ),
        (
            "CORS preflight",
            Box::new(|p| {
                let r = mfetch(
                    p,
                    ports,
                    "OPTIONS",
                    "/cors",
                    &[
                        ("origin", "https://app.example.com"),
                        ("access-control-request-method", "POST"),
                    ],
                    b"",
                )?;
                check!(
                    r.status == 200 || r.status == 204,
                    "status {} {:?}",
                    r.status,
                    r.headers
                );
                check!(
                    r.header("access-control-allow-origin") == Some("https://app.example.com"),
                    "allow-origin {:?}",
                    r.headers
                );
                let g = get(p, "/cors", &[("origin", "https://app.example.com")])?;
                status_is(&g, 200)?;
                check!(
                    g.header("access-control-allow-origin") == Some("https://app.example.com"),
                    "allow-origin on GET {:?}",
                    g.headers
                );
                Ok(())
            }),
        ),
        (
            "fault injection (abort 503)",
            Box::new(|p| {
                let r = get(p, "/fault", &[])?;
                status_is(&r, 503)?;
                Ok(())
            }),
        ),
        (
            "no route (404)",
            Box::new(|p| {
                let r = get(p, "/no/such/route", &[])?;
                status_is(&r, 404)?;
                Ok(())
            }),
        ),
        (
            "metrics count requests",
            Box::new(|p| {
                let before = requests_total(ports)?;
                for _ in 0..3 {
                    status_is(&get(p, "/respond", &[])?, 200)?;
                }
                // 計数は応答の後なので少し待つ
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    let after = requests_total(ports)?;
                    if after - before >= 3.0 {
                        return Ok(());
                    }
                    if Instant::now() > deadline {
                        return Err(format!("requests_total {before} -> {after}"));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }),
        ),
        (
            "access log",
            Box::new(|p| {
                let path = format!("/al/{}", p.name());
                status_is(&get(p, &path, &[])?, 200)?;
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    let log = std::fs::read_to_string(&access_log).unwrap_or_default();
                    if log.lines().any(|l| l.contains(&format!("\"{path}\""))) {
                        return Ok(());
                    }
                    if Instant::now() > deadline {
                        return Err(format!(
                            "{path} not in the access log ({} lines)",
                            log.lines().count()
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }),
        ),
    ];

    let mut failures = Vec::new();
    let mut table = String::new();
    table.push_str("| feature |");
    for p in &protos {
        table.push_str(&format!(" {} |", p.name()));
    }
    table.push_str("\n|---|");
    for _ in &protos {
        table.push_str("---|");
    }
    table.push('\n');
    // 文書にある制限で、機能が働かないことを確かめている組み（表では n/a と脚注）
    let limited = |name: &str, p: MProto| name.starts_with("proxy cache") && !p.is_h1();
    for (name, case) in &cases {
        table.push_str(&format!("| {name} |"));
        for p in &protos {
            match case(*p) {
                Ok(()) if limited(name, *p) => table.push_str(" n/a¹ |"),
                Ok(()) => table.push_str(" ok |"),
                Err(e) => {
                    table.push_str(" **FAIL** |");
                    failures.push(format!("{name} / {}: {e}", p.name()));
                }
            }
        }
        table.push('\n');
    }
    table.push_str(
        "\n¹ The proxy cache serves HTTP/1.1 clients only (documented limitation); \
         the cell checks that the response is relayed uncached.\n",
    );
    println!("{table}");
    assert!(
        failures.is_empty(),
        "{} failure(s):\n{}\n\n{table}\n{}",
        failures.len(),
        failures.join("\n"),
        veil.log()
    );
}
