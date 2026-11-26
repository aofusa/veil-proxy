//! F-329: Proxy-Wasm のコールバックが要求ごとに 1 回ずつ呼ばれることの E2E（全プロトコル）。
//!
//! 1 モジュール・`instance_reuse` のルートでローカル応答を返す要求は、作成・ヘッダ・本文・ログ・削除の段を 1 回の
//! ファイバーの出入りで呼ぶ（続けて呼ぶ経路）。トレーラ付きの本文・`Expect: 100-continue` は段ごとに呼ぶ経路のまま。
//! どちらの経路でも、要求ごとに作成・ヘッダ・ログ・削除が 1 回ずつ、本文の段は本文のある要求だけ全量で 1 回
//! （トレーラ付きは段ごと）で、ヘッダの段の間は本文が見えないことを、モジュールが数えた回数で確かめる。上限を超える
//! 本文の 413 は、HTTP/2・HTTP/3 ではモジュールを呼ばずに返し、HTTP/1.1 の chunked ではヘッダの段の後に返す（ログの段は
//! 呼ばない。B-504）。インスタンスはリスナーのワーカーごと（平文と TLS で別）なので、回数はプロトコルごとの差で見る。

use super::standalone::parity::*;
use super::standalone::*;
use std::io::{Read, Write};
use std::time::{Duration, Instant};

/// 自分の呼ばれた回数を数え、`/stats` にその回数（7 個の i32、リトルエンディアン）を返すモジュール。
/// 本文の無い要求はヘッダの段で、本文のある要求は本文の終わり（`end_of_stream`）で `ok` を返す
const COUNTER: &str = r#"
(module
  (import "env" "proxy_get_property"
    (func $prop (param i32 i32 i32 i32) (result i32)))
  (import "env" "proxy_get_buffer_bytes"
    (func $buffer (param i32 i32 i32 i32 i32) (result i32)))
  (import "env" "proxy_send_local_response"
    (func $slr (param i32 i32 i32 i32 i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 4)
  (data (i32.const 16) "request\00path")
  (data (i32.const 32) "/stats")
  (data (i32.const 48) "ok")
  (global $bump (mut i32) (i32.const 65536))
  (global $creates (mut i32) (i32.const 0))
  (global $headers (mut i32) (i32.const 0))
  (global $bodies (mut i32) (i32.const 0))
  (global $body_bytes (mut i32) (i32.const 0))
  (global $logs (mut i32) (i32.const 0))
  (global $deletes (mut i32) (i32.const 0))
  (global $early (mut i32) (i32.const 0))
  (func (export "proxy_on_memory_allocate") (param $size i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get $size)))
    (local.get $p))
  (func (export "proxy_on_vm_start") (param i32 i32) (result i32) (i32.const 1))
  (func (export "proxy_on_configure") (param i32 i32) (result i32) (i32.const 1))
  (func (export "proxy_on_context_create") (param $ctx i32) (param $parent i32)
    (if (local.get $parent)
      (then
        (global.set $creates (i32.add (global.get $creates) (i32.const 1)))
        ;; 要求は 1 つずつ来るので、確保は要求ごとに始めからやり直す
        (global.set $bump (i32.const 65536)))))
  (func $is_stats (result i32)
    (local $p i32)
    (i32.store (i32.const 4) (i32.const 0))
    (if (call $prop (i32.const 16) (i32.const 12) (i32.const 0) (i32.const 4))
      (then (return (i32.const 0))))
    (if (i32.ne (i32.load (i32.const 4)) (i32.const 6))
      (then (return (i32.const 0))))
    (local.set $p (i32.load (i32.const 0)))
    (i32.and
      (i32.eq (i32.load (local.get $p)) (i32.load (i32.const 32)))
      (i32.eq (i32.load16_u (i32.add (local.get $p) (i32.const 4)))
              (i32.load16_u (i32.const 36)))))
  (func $ok
    (drop (call $slr (i32.const 200) (i32.const 0) (i32.const 0) (i32.const 48) (i32.const 2)
                     (i32.const 0) (i32.const 0) (i32.const 0))))
  (func (export "proxy_on_request_headers") (param $ctx i32) (param $n i32) (param $eos i32) (result i32)
    (global.set $headers (i32.add (global.get $headers) (i32.const 1)))
    ;; ヘッダの段の間は本文が見えない
    (i32.store (i32.const 12) (i32.const 0))
    (if (i32.eqz (call $buffer (i32.const 0) (i32.const 0) (i32.const 1048576) (i32.const 8) (i32.const 12)))
      (then
        (if (i32.load (i32.const 12))
          (then (global.set $early (i32.add (global.get $early) (i32.const 1)))))))
    (if (call $is_stats)
      (then
        (i32.store (i32.const 64) (global.get $creates))
        (i32.store (i32.const 68) (global.get $headers))
        (i32.store (i32.const 72) (global.get $bodies))
        (i32.store (i32.const 76) (global.get $body_bytes))
        (i32.store (i32.const 80) (global.get $logs))
        (i32.store (i32.const 84) (global.get $deletes))
        (i32.store (i32.const 88) (global.get $early))
        (drop (call $slr (i32.const 200) (i32.const 0) (i32.const 0) (i32.const 64) (i32.const 28)
                         (i32.const 0) (i32.const 0) (i32.const 0)))
        (return (i32.const 1))))
    (if (local.get $eos)
      (then
        (call $ok)
        (return (i32.const 1))))
    (i32.const 0))
  (func (export "proxy_on_request_body") (param $ctx i32) (param $size i32) (param $eos i32) (result i32)
    (global.set $bodies (i32.add (global.get $bodies) (i32.const 1)))
    (if (local.get $eos)
      (then
        (global.set $body_bytes (i32.add (global.get $body_bytes) (local.get $size)))
        (call $ok)
        (return (i32.const 1))))
    (i32.const 0))
  (func (export "proxy_on_log") (param $ctx i32)
    (global.set $logs (i32.add (global.get $logs) (i32.const 1))))
  (func (export "proxy_on_delete") (param $ctx i32)
    (global.set $deletes (i32.add (global.get $deletes) (i32.const 1))))
)
"#;

/// モジュールが数えた回数（1 つのインスタンスの、起動からの合計）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Counts {
    creates: i32,
    headers: i32,
    bodies: i32,
    body_bytes: i32,
    logs: i32,
    deletes: i32,
    early: i32,
}

fn counts(proto: Proto, ports: Ports, veil: &VeilProcess) -> Counts {
    let r = fetch(proto, ports, "GET", "/stats", &[], b"");
    assert_eq!(r.status, 200, "{proto:?} /stats\n{}", veil.log());
    assert_eq!(r.body.len(), 28, "{proto:?} /stats {:?}", r.text());
    let v = |i: usize| i32::from_le_bytes(r.body[i * 4..i * 4 + 4].try_into().unwrap());
    Counts {
        creates: v(0),
        headers: v(1),
        bodies: v(2),
        body_bytes: v(3),
        logs: v(4),
        deletes: v(5),
        early: v(6),
    }
}

/// 前の要求が全部ログ・削除の段を終えた時点の回数（この `/stats` の要求自身は作成・ヘッダの段だけが数に入る）。
/// 段ごとに呼ぶ経路のログの段は応答の後に走るので、揃うまで `/stats` を送り直す。送り直した数を `resent` に足す。
/// `unlogged` はそれまでにログの段を呼ばずに終えた要求の数（veil がヘッダの段の後に 413 を返した要求。B-504）
fn settled(
    proto: Proto,
    ports: Ports,
    veil: &VeilProcess,
    resent: &mut i32,
    unlogged: i32,
) -> Counts {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let c = counts(proto, ports, veil);
        assert!(
            c.logs < c.creates && c.deletes < c.creates && c.headers == c.creates,
            "{proto:?}: a callback ran more than once per request: {c:?}\n{}",
            veil.log()
        );
        if c.logs == c.creates - 1 - unlogged && c.deletes == c.creates - 1 {
            return c;
        }
        assert!(
            Instant::now() < deadline,
            "{proto:?}: the log and delete phases did not run once per request: {c:?}\n{}",
            veil.log()
        );
        *resent += 1;
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// HTTP/1.1 の要求をそのまま送って応答を読む（`Connection: close`）。`expect_continue` なら本文の前に
/// `100 Continue` を待ってから送り、最終の応答だけを返す
fn h1_raw(port: u16, head: &str, body: &[u8], expect_continue: bool) -> HttpResponse {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(head.as_bytes()).unwrap();
    let mut raw = Vec::new();
    if expect_continue {
        let mut buf = [0u8; 1024];
        while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = s.read(&mut buf).unwrap();
            assert!(
                n > 0,
                "closed before 100 Continue: {:?}",
                String::from_utf8_lossy(&raw)
            );
            raw.extend_from_slice(&buf[..n]);
        }
        assert!(
            raw.starts_with(b"HTTP/1.1 100 "),
            "expected 100 Continue: {:?}",
            String::from_utf8_lossy(&raw)
        );
        let end = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        raw.drain(..end);
    }
    // 上限を超える本文は応答の後に閉じられて書けないことがある
    let _ = s.write_all(body);
    let _ = s.read_to_end(&mut raw);
    parse_http1_response(&raw).unwrap()
}

/// 本文 `body` とトレーラ（`x-t: 1`）を送る POST（HTTP/2・HTTP/3）
fn post_with_trailers(
    proto: Proto,
    ports: Ports,
    body: &'static [u8],
) -> Result<HttpResponse, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut trailers = http::HeaderMap::new();
    trailers.insert("x-t", http::HeaderValue::from_static("1"));
    rt.block_on(async move {
        tokio::time::timeout(Duration::from_secs(15), async move {
            match proto {
                Proto::H2c | Proto::H2Tls => {
                    let tls = proto == Proto::H2Tls;
                    let port = if tls { ports.tls } else { ports.clear };
                    let mut client = h2_connect(tls, port).await?;
                    let req = h2_request(tls, port, "POST", "/a", &[]);
                    let (resp, mut send) =
                        client.send_request(req, false).map_err(|e| e.to_string())?;
                    send.send_data(bytes::Bytes::from_static(body), false)
                        .map_err(|e| e.to_string())?;
                    send.send_trailers(trailers).map_err(|e| e.to_string())?;
                    let resp = resp.await.map_err(|e| e.to_string())?;
                    let status = resp.status().as_u16();
                    let mut r = HttpResponse {
                        status,
                        headers: Vec::new(),
                        body: Vec::new(),
                    };
                    let mut b = resp.into_body();
                    while let Some(c) = b.data().await {
                        let c = c.map_err(|e| e.to_string())?;
                        let _ = b.flow_control().release_capacity(c.len());
                        r.body.extend_from_slice(&c);
                    }
                    Ok(r)
                }
                #[cfg(feature = "http3")]
                Proto::H3 => {
                    use bytes::Buf;
                    let (_endpoint, mut send) = h3_connect(ports.tls).await?;
                    let req = h3_request(ports.tls, "POST", "/a", &[]);
                    let mut stream = send.send_request(req).await.map_err(|e| e.to_string())?;
                    stream
                        .send_data(bytes::Bytes::from_static(body))
                        .await
                        .map_err(|e| e.to_string())?;
                    stream
                        .send_trailers(trailers)
                        .await
                        .map_err(|e| e.to_string())?;
                    stream.finish().await.map_err(|e| e.to_string())?;
                    let resp = stream.recv_response().await.map_err(|e| e.to_string())?;
                    let mut r = HttpResponse {
                        status: resp.status().as_u16(),
                        headers: Vec::new(),
                        body: Vec::new(),
                    };
                    while let Some(mut c) = stream.recv_data().await.map_err(|e| e.to_string())? {
                        let n = c.remaining();
                        r.body.extend_from_slice(&c.copy_to_bytes(n));
                    }
                    Ok(r)
                }
                _ => Err(format!("trailers are not sent over {proto:?}")),
            }
        })
        .await
        .map_err(|_| "timed out".to_string())?
    })
}

/// F-329: 続けて呼ぶ経路・段ごとに呼ぶ経路のどちらでも、要求ごとに作成・ヘッダ・ログ・削除の段が 1 回ずつ、
/// 本文の段は本文の全量で 1 回、ヘッダの段の間は本文が見えない（HTTP/1.1・h2c・HTTP/2・HTTP/3）
#[test]
fn f329_wasm_callbacks_run_once_per_request_on_every_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    let wat = dir.path().join("counter.wat");
    std::fs::write(&wat, COUNTER).unwrap();
    let ports = Ports::alloc();
    let config = format!(
        r#"{listeners}
[wasm]
enabled = true

[[wasm.modules]]
name = "counter"
path = "{wat}"
instance_reuse = true
[wasm.modules.capabilities]
allow_send_local_response = true
allow_request_body_read = true
allowed_properties = ["request.path"]

[[route]]
modules = ["counter"]
[route.conditions]
path = "/*"
[route.action]
type = "Respond"
status = 200
body = "fixed"
[route.security]
max_request_body_size = 1024
max_chunked_body_size = 1024
"#,
        listeners = listeners_toml(ports, &cert, &key),
        wat = tp(&wat),
    );
    let veil = spawn_config(dir.path(), ports, &config);
    let ok = |proto: Proto, what: &str, r: &HttpResponse| {
        assert_eq!(
            (r.status, r.text().as_str()),
            (200, "ok"),
            "{proto:?} {what}\n{}",
            veil.log()
        );
    };
    let chunked = "POST /a HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
                   Transfer-Encoding: chunked\r\n\r\n";
    for proto in all_protos() {
        let before = settled(proto, ports, &veil, &mut 0, 0);
        let mut resent = 0;
        // 要求の数、本文の段の数（本文の全量で 1 回）、本文の段が受けた大きさの合計
        let (mut requests, mut bodies, mut bytes) = (0, 0, 0);
        ok(
            proto,
            "GET",
            &fetch(proto, ports, "GET", "/a?x=1", &[], b""),
        );
        requests += 1;
        ok(
            proto,
            "POST",
            &fetch(proto, ports, "POST", "/a", &[], b"hello world"),
        );
        (requests, bodies, bytes) = (requests + 1, bodies + 1, bytes + 11);
        if proto == Proto::H1 {
            ok(
                proto,
                "chunked",
                &h1_raw(
                    ports.clear,
                    chunked,
                    b"5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
                    false,
                ),
            );
            (requests, bodies, bytes) = (requests + 1, bodies + 1, bytes + 11);
            ok(
                proto,
                "empty chunked",
                &h1_raw(ports.clear, chunked, b"0\r\n\r\n", false),
            );
            (requests, bodies) = (requests + 1, bodies + 1);
            let expect = "POST /a HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
                          Content-Length: 5\r\nExpect: 100-continue\r\n\r\n";
            ok(
                proto,
                "100-continue",
                &h1_raw(ports.clear, expect, b"hello", true),
            );
            (requests, bodies, bytes) = (requests + 1, bodies + 1, bytes + 5);
        }
        let after = settled(proto, ports, &veil, &mut resent, 0);
        // `before` の後の要求と、`after` の `/stats`（送り直した分を含む）
        let created = requests + resent + 1;
        assert_eq!(
            (
                after.creates - before.creates,
                after.bodies - before.bodies,
                after.body_bytes - before.body_bytes,
                after.early,
            ),
            (created, bodies, bytes, 0),
            "{proto:?}: (creates, request bodies, body bytes, body seen in the headers phase) \
             {before:?} -> {after:?}\n{}",
            veil.log()
        );
        if proto == Proto::H1 {
            continue;
        }
        // トレーラ付きの本文（段ごとに呼ぶ経路。本文の段は本文の終わりを受けないので、ルートの固定応答）
        let before = settled(proto, ports, &veil, &mut 0, 0);
        let mut resent = 0;
        let r = post_with_trailers(proto, ports, b"hello")
            .unwrap_or_else(|e| panic!("{proto:?} trailers: {e}\n{}", veil.log()));
        assert_eq!(r.status, 200, "{proto:?} trailers\n{}", veil.log());
        let after = settled(proto, ports, &veil, &mut resent, 0);
        assert_eq!(
            (after.creates - before.creates, after.early),
            (resent + 2, 0),
            "{proto:?} trailers {before:?} -> {after:?}\n{}",
            veil.log()
        );
        assert!(
            after.bodies > before.bodies,
            "{proto:?} trailers: no request-body phase {before:?} -> {after:?}"
        );
        // 上限（`max_request_body_size` = 1024）を超える本文は 413。本文を溜めてからヘッダの段を呼ぶので、モジュールは
        // 呼ばない
        let before = after;
        let mut resent = 0;
        let r = fetch(proto, ports, "POST", "/a", &[], &[b'x'; 2048]);
        assert_eq!(r.status, 413, "{proto:?} over the limit\n{}", veil.log());
        let after = settled(proto, ports, &veil, &mut resent, 0);
        assert_eq!(
            (after.creates - before.creates, after.bodies - before.bodies),
            (resent + 1, 0),
            "{proto:?} over the limit {before:?} -> {after:?}\n{}",
            veil.log()
        );
    }
    // HTTP/1.1 で上限（`max_chunked_body_size` = 1024）を超える chunked は 413。ヘッダの段は本文を読む前に呼ぶので
    // context は作られ、本文の段もログの段も呼ばれずに、次にインスタンスを使うときに消える（B-504）。このインスタンスの
    // ログの数は以後 1 つ少ないので最後に確かめる
    let proto = Proto::H1;
    let before = settled(proto, ports, &veil, &mut 0, 0);
    let mut resent = 0;
    let mut big = Vec::new();
    for _ in 0..2 {
        big.extend_from_slice(b"400\r\n");
        big.extend_from_slice(&[b'x'; 1024]);
        big.extend_from_slice(b"\r\n");
    }
    big.extend_from_slice(b"0\r\n\r\n");
    let r = h1_raw(ports.clear, chunked, &big, false);
    assert_eq!(r.status, 413, "over the limit\n{}", veil.log());
    let after = settled(proto, ports, &veil, &mut resent, 1);
    assert_eq!(
        (
            after.creates - before.creates,
            after.headers - before.headers,
            after.bodies - before.bodies,
            after.logs - before.logs,
            after.deletes - before.deletes,
        ),
        (resent + 2, resent + 2, 0, resent + 1, resent + 2),
        "over the limit: (creates, headers, bodies, logs, deletes) {before:?} -> {after:?}\n{}",
        veil.log()
    );
}
