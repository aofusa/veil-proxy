//! 機能 × プロトコルの網羅性の確認（feat/protocol-parity）で見つけた差分の回帰試験（B-481〜B-488・F-312〜F-316）。
//!
//! 同じ設定の veil に HTTP/1.1（平文）・h2c・HTTP/2（TLS）・HTTP/3 で同じ要求を送り、上流が受けたもの・応答を比べる。

use super::standalone::parity::*;
use super::standalone::*;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 本文を HEADERS と別に（少し後に）送る POST（HTTP/2・HTTP/3 で本文が後から届く要求にする。同じ読み取りで本文の
/// 全量が見えると HTTP/2 は溜める経路で起動する。F-298）。応答のステータス。本文を送る前に断られる要求もあるので、
/// 本文の送信の失敗（相手のリセット・STOP_SENDING）は無視して応答を読む
fn post_body_later(
    proto: Proto,
    ports: Ports,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<u16, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let path = path.to_string();
    let headers: Vec<(String, String)> = headers
        .iter()
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .collect();
    let body = bytes::Bytes::copy_from_slice(body);
    rt.block_on(async move {
        let run = async {
            match proto {
                Proto::H2c | Proto::H2Tls => {
                    let tls = proto == Proto::H2Tls;
                    let port = if tls { ports.tls } else { ports.clear };
                    let mut client = h2_connect(tls, port).await?;
                    let req = h2_request(tls, port, "POST", &path, &headers);
                    let (resp, mut send) =
                        client.send_request(req, false).map_err(|e| e.to_string())?;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let _ = send.send_data(body, true);
                    let resp = resp.await.map_err(|e| e.to_string())?;
                    let status = resp.status().as_u16();
                    let mut b = resp.into_body();
                    while let Some(c) = b.data().await {
                        let c = c.map_err(|e| e.to_string())?;
                        let _ = b.flow_control().release_capacity(c.len());
                    }
                    Ok(status)
                }
                #[cfg(feature = "http3")]
                Proto::H3 => {
                    let hdrs: Vec<(&str, &str)> = headers
                        .iter()
                        .map(|(n, v)| (n.as_str(), v.as_str()))
                        .collect();
                    let (_endpoint, mut send) = h3_connect(ports.tls).await?;
                    let req = h3_request(ports.tls, "POST", &path, &hdrs);
                    let mut stream = send.send_request(req).await.map_err(|e| e.to_string())?;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if stream.send_data(body).await.is_ok() {
                        let _ = stream.finish().await;
                    }
                    let resp = stream.recv_response().await.map_err(|e| e.to_string())?;
                    // 応答の後に STOP_SENDING・リセットで閉じられることがある（応答は受け取れている）
                    while let Ok(Some(_)) = stream.recv_data().await {}
                    Ok(resp.status().as_u16())
                }
                _ => Err(format!("{proto:?} is not supported")),
            }
        };
        tokio::time::timeout(Duration::from_secs(15), run)
            .await
            .unwrap_or_else(|_| Err("timed out".to_string()))
    })
}

fn seen_len(s: &Arc<Mutex<Vec<UpstreamSeen>>>) -> usize {
    s.lock().unwrap().len()
}

/// B-481: ラウンドロビンの 2 台へ、HTTP/2・HTTP/3 で本文を後から送る要求（ストリーミングの中継・gRPC を HTTP/1.1 の
/// 上流へ送るバッファ経路）を続けて送ると、両方の上流へ交互に届く（以前は HTTP/2 の下見と本番・HTTP/3 の分類と
/// バッファ経路で 2 回選び、カウンタが 1 要求で 2 つ進んで全部が同じ上流へ行った）
#[test]
fn b481_round_robin_alternates_for_streaming_requests() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    let (a, seen_a) = spawn_recording_upstream(|_| (200, String::new(), b"a".to_vec()));
    let (b, seen_b) = spawn_recording_upstream(|_| (200, String::new(), b"b".to_vec()));
    let ports = Ports::alloc();
    let config = format!(
        r#"{listeners}
[upstreams."rr"]
algorithm = "round_robin"
servers = ["http://127.0.0.1:{a}", "http://127.0.0.1:{b}"]

[[route]]
[route.conditions]
path = "/rr/*"
[route.action]
type = "Proxy"
upstream = "rr"
"#,
        listeners = listeners_toml(ports, &cert, &key),
    );
    let veil = spawn_config(dir.path(), ports, &config);
    for proto in all_protos().into_iter().filter(|p| *p != Proto::H1) {
        for (path, headers) in [
            ("/rr/upload", &[][..]),
            ("/rr/grpc", &[("content-type", "application/grpc")][..]),
        ] {
            let (a0, b0) = (seen_len(&seen_a), seen_len(&seen_b));
            for _ in 0..4 {
                let r = post_body_later(proto, ports, path, headers, b"hello");
                assert!(r.is_ok(), "{proto:?} {path}: {r:?}\n{}", veil.log());
            }
            let (da, db) = (seen_len(&seen_a) - a0, seen_len(&seen_b) - b0);
            assert_eq!(
                (da, db),
                (2, 2),
                "{proto:?} {path}: upstream a={da} b={db}\n{}",
                veil.log()
            );
        }
    }
}

/// HTTP/1.1 の WebSocket の upgrade の要求を送り、応答の始めを返す（上流の選択だけを見るので握手は完了させない）
fn h1_ws_upgrade(ports: Ports, path: &str, headers: &[(&str, &str)]) -> Result<String, String> {
    use std::io::{Read, Write};
    let mut s = connect_local(ports.clear, Duration::from_secs(2)).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    let mut req = format!(
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
    );
    for (n, v) in headers {
        req.push_str(&format!("{n}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 256];
    let n = s.read(&mut buf).map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&buf[..n]).into_owned())
}

/// HTTP/2 の拡張 CONNECT（`:protocol = websocket`。RFC 8441）を開いて応答のステータスを返す
fn h2_ws_connect(
    tls: bool,
    ports: Ports,
    path: &str,
    headers: &[(&str, &str)],
) -> Result<u16, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let path = path.to_string();
    let headers: Vec<(String, String)> = headers
        .iter()
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .collect();
    rt.block_on(async move {
        let run = async {
            let port = if tls { ports.tls } else { ports.clear };
            let client = h2_connect(tls, port).await?;
            let mut client = client.ready().await.map_err(|e| e.to_string())?;
            let mut waited = 0;
            // h2 は SETTINGS_ENABLE_CONNECT_PROTOCOL を受けるまで `:protocol` を送れない
            while !client.is_extended_connect_protocol_enabled() {
                if waited > 100 {
                    return Err("extended CONNECT was not advertised".to_string());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
                waited += 1;
                client = client.ready().await.map_err(|e| e.to_string())?;
            }
            let scheme = if tls { "https" } else { "http" };
            let mut b = http::Request::builder()
                .method(http::Method::CONNECT)
                .uri(format!("{scheme}://localhost:{port}{path}"))
                .extension(h2::ext::Protocol::from_static("websocket"))
                .header("sec-websocket-version", "13");
            for (n, v) in &headers {
                b = b.header(n.as_str(), v.as_str());
            }
            let (resp, _send) = client
                .send_request(b.body(()).unwrap(), false)
                .map_err(|e| e.to_string())?;
            let resp = resp.await.map_err(|e| e.to_string())?;
            Ok(resp.status().as_u16())
        };
        tokio::time::timeout(Duration::from_secs(15), run)
            .await
            .unwrap_or_else(|_| Err("timed out".to_string()))
    })
}

/// 上流 a・b のどちらが要求を 1 つ受けたか（`send` の前後の数で見る）
fn which_upstream(
    seen_a: &Arc<Mutex<Vec<UpstreamSeen>>>,
    seen_b: &Arc<Mutex<Vec<UpstreamSeen>>>,
    send: impl FnOnce(),
) -> &'static str {
    let (a0, b0) = (seen_len(seen_a), seen_len(seen_b));
    send();
    let ok = wait_until(Duration::from_secs(10), || {
        seen_len(seen_a) + seen_len(seen_b) > a0 + b0
    });
    assert!(ok, "no upstream saw the request");
    match (seen_len(seen_a) - a0, seen_len(seen_b) - b0) {
        (1, 0) => "a",
        (0, 1) => "b",
        other => panic!("unexpected upstream hits {other:?}"),
    }
}

/// B-485: Consistent Hash の `cookie:` キーで、同じ Cookie の HTTP の要求と WebSocket（HTTP/1.1 の upgrade・
/// HTTP/2 の拡張 CONNECT）が同じ上流へ行く（以前は WebSocket だけ送信元の IP で選んだ）。HTTP/3 の拡張 CONNECT は
/// HTTP/2 と同じ中継の関数（`ws_tunnel`）を通る（テストの h3 クライアントは `websocket` を送れない）
#[test]
fn b485_websocket_follows_consistent_hash_cookie() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    let (a, seen_a) = spawn_recording_upstream(|_| (200, String::new(), b"a".to_vec()));
    let (b, seen_b) = spawn_recording_upstream(|_| (200, String::new(), b"b".to_vec()));
    let ports = Ports::alloc();
    let config = format!(
        r#"{listeners}
[http2]
enable_connect_protocol = true

[upstreams."ch"]
algorithm = "consistent_hash"
hash_key = "cookie:sid"
servers = ["http://127.0.0.1:{a}", "http://127.0.0.1:{b}"]

[[route]]
[route.conditions]
path = "/app/*"
[route.action]
type = "Proxy"
upstream = "ch"
"#,
        listeners = listeners_toml(ports, &cert, &key),
    );
    let veil = spawn_config(dir.path(), ports, &config);
    let mut used = std::collections::HashSet::new();
    for i in 0..12 {
        let cookie = format!("sid=user{i}");
        let headers = [("cookie", cookie.as_str())];
        let http = which_upstream(&seen_a, &seen_b, || {
            let r = fetch(Proto::H1, ports, "GET", "/app/page", &headers, b"");
            assert_eq!(r.status, 200, "{}", veil.log());
        });
        used.insert(http);
        let ws = which_upstream(&seen_a, &seen_b, || {
            let r = h1_ws_upgrade(ports, "/app/ws", &headers);
            assert!(r.is_ok(), "{r:?}\n{}", veil.log());
        });
        assert_eq!(ws, http, "HTTP/1.1 WebSocket {cookie}\n{}", veil.log());
        for tls in [false, true] {
            let ws = which_upstream(&seen_a, &seen_b, || {
                let r = h2_ws_connect(tls, ports, "/app/ws", &headers);
                assert!(r.is_ok(), "{r:?}\n{}", veil.log());
            });
            assert_eq!(
                ws,
                http,
                "HTTP/2 (tls={tls}) extended CONNECT {cookie}\n{}",
                veil.log()
            );
        }
    }
    // Cookie で分かれていること（全部が同じ上流なら送信元の IP で選んでも通ってしまう）
    assert_eq!(used.len(), 2, "all cookies hashed to one upstream");
}

/// `head`（HTTP/1.1 の要求の head）の `Via` の行の値を全部返す（名前の大文字小文字を区別しない）
fn via_lines(head: &str) -> Vec<String> {
    head.lines()
        .filter_map(|l| {
            let (n, v) = l.split_once(':')?;
            n.trim()
                .eq_ignore_ascii_case("via")
                .then(|| v.trim().to_string())
        })
        .collect()
}

/// B-484: `via_header_enabled` の `Via` が、どのクライアントのプロトコルでも上流（HTTP/1.1・h2c）への要求と WebSocket
/// の upgrade に 1 つだけ付き、版が受けたプロトコルに合う（以前は HTTP/1.1 のクライアント → HTTP/1.1 の上流だけ）
#[test]
fn b484_via_header_on_every_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    let (up1, seen1) = spawn_recording_upstream(|_| (200, String::new(), b"ok".to_vec()));
    let (up2, seen2) = spawn_recording_h2c_upstream();
    let ports = Ports::alloc();
    let config = format!(
        r#"{listeners}
[http2]
enable_connect_protocol = true

[performance]
via_header_enabled = true
via_header_hostname = "edge-1"

[[route]]
[route.conditions]
path = "/h1/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up1}"

[[route]]
[route.conditions]
path = "/h2/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up2}"
use_h2c = true
"#,
        listeners = listeners_toml(ports, &cert, &key),
    );
    let veil = spawn_config(dir.path(), ports, &config);
    let last_h1 = || seen1.lock().unwrap().last().unwrap().head.clone();
    let last_h2 = || {
        let s = seen2.lock().unwrap();
        let h = &s.last().unwrap().headers;
        h.iter()
            .filter(|(n, _)| n == "via")
            .map(|(_, v)| v.clone())
            .collect::<Vec<_>>()
    };
    for proto in all_protos() {
        let want = match proto {
            Proto::H1 => "1.1 edge-1",
            Proto::H2c | Proto::H2Tls => "2 edge-1",
            Proto::H3 => "3 edge-1",
        };
        let ctx = || format!("{proto:?}\n{}", veil.log());
        let r = fetch(proto, ports, "GET", "/h1/a", &[], b"");
        assert_eq!(r.status, 200, "{}", ctx());
        assert_eq!(via_lines(&last_h1()), [want], "{}", ctx());
        let r = fetch(proto, ports, "GET", "/h2/a", &[], b"");
        assert_eq!(r.status, 200, "{}", ctx());
        assert_eq!(last_h2(), [want], "{}", ctx());
        if proto != Proto::H1 {
            // 本文が後から届く要求（ストリーミングの中継）
            let r = post_body_later(proto, ports, "/h1/up", &[], b"body");
            assert_eq!(r, Ok(200), "{}", ctx());
            assert_eq!(via_lines(&last_h1()), [want], "{}", ctx());
            let r = post_body_later(proto, ports, "/h2/up", &[], b"body");
            assert_eq!(r, Ok(200), "{}", ctx());
            assert_eq!(last_h2(), [want], "{}", ctx());
        }
    }
    // WebSocket の upgrade（HTTP/1.1・HTTP/2 の拡張 CONNECT）
    let r = h1_ws_upgrade(ports, "/h1/ws", &[]);
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(via_lines(&last_h1()), ["1.1 edge-1"], "{}", veil.log());
    let r = h2_ws_connect(false, ports, "/h1/ws", &[]);
    assert!(r.is_ok(), "{r:?}");
    assert_eq!(via_lines(&last_h1()), ["2 edge-1"], "{}", veil.log());
}

/// B-483: 管理 API（`/__admin/*`）とキャッシュの PURGE が HTTP/3 でも HTTP/1.1・HTTP/2 と同じに応え、全部を受ける
/// Proxy のルートへ流れない（以前は HTTP/3 だけ普通のルートへ流れ、`Authorization` ごと上流へ送られた）。本文が後から
/// 届く要求（HTTP/2・HTTP/3 のストリーミングの経路）も同じ
#[test]
#[cfg(feature = "admin")]
fn b483_admin_api_on_every_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    let (up, seen) = spawn_recording_upstream(|_| (200, String::new(), b"upstream".to_vec()));
    let ports = Ports::alloc();
    let config = format!(
        r#"{listeners}
[admin]
enabled = true
secret = "s3cret"

[[route]]
[route.conditions]
path = "/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up}"
"#,
        listeners = listeners_toml(ports, &cert, &key),
    );
    let veil = spawn_config(dir.path(), ports, &config);
    let auth = [("authorization", "Bearer s3cret")];
    for proto in all_protos() {
        let ctx = || format!("{proto:?}\n{}", veil.log());
        let r = fetch(proto, ports, "GET", "/__admin/stats", &auth, b"");
        assert_eq!(r.status, 200, "{}", ctx());
        assert!(r.text().contains("uptime_secs"), "{} {}", r.text(), ctx());
        let r = fetch(proto, ports, "GET", "/__admin/stats", &[], b"");
        assert_eq!(r.status, 401, "{}", ctx());
        // PURGE（キャッシュの無い構成は 503。上流へは送らない）
        let r = fetch(proto, ports, "PURGE", "/some/page", &auth, b"");
        assert_ne!(r.text(), "upstream", "{}", ctx());
        if proto != Proto::H1 {
            let r = post_body_later(proto, ports, "/__admin/reload", &auth, b"{}");
            assert_eq!(r, Ok(200), "{}", ctx());
        }
        // 管理 API の要求は上流へ届かない（普通の要求は届く）
        let r = fetch(proto, ports, "GET", "/page", &[], b"");
        assert_eq!(r.text(), "upstream", "{}", ctx());
        let heads: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.head.clone())
            .collect();
        assert!(
            heads
                .iter()
                .all(|h| !h.contains("__admin") && !h.starts_with("PURGE")),
            "{heads:?} {}",
            ctx()
        );
    }
}

/// B-482: 要求ヘッダの上限（veil 全体の 8 KB・64 フィールド・`:path` の 8 KB、ルートの `max_request_header_size`）が
/// HTTP/1.1・HTTP/2・HTTP/3 で同じに効く（以前は HTTP/2 が 8 KB を見ず、フィールドの数は HTTP/1.1 だけが 400 で断り、
/// ルートの上限はどのプロトコルも見ていなかった）。本文が後から届く要求（HTTP/2 の下見・HTTP/3 の分類）も同じ
#[test]
fn b482_request_header_limits_on_every_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    let (up, seen) = spawn_recording_upstream(|_| (200, String::new(), b"ok".to_vec()));
    let ports = Ports::alloc();
    let config = format!(
        r#"{listeners}
[[route]]
[route.conditions]
path = "/small/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up}"
[route.security]
max_request_header_size = 1024

[[route]]
[route.conditions]
path = "/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up}"
"#,
        listeners = listeners_toml(ports, &cert, &key),
    );
    let veil = spawn_config(dir.path(), ports, &config);
    let big = "a".repeat(10 * 1024);
    let mid = "b".repeat(2 * 1024);
    let many: Vec<(String, String)> = (0..65)
        .map(|i| (format!("x-h{i}"), "v".to_string()))
        .collect();
    let many: Vec<(&str, &str)> = many.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
    let long_path = format!("/{}", "p".repeat(9 * 1024));
    for proto in all_protos() {
        let ctx = || format!("{proto:?}\n{}", veil.log());
        let before = seen_len(&seen);
        let r = fetch(proto, ports, "GET", "/page", &[("x-big", &big)], b"");
        assert_eq!(r.status, 431, "10 KB header: {}", ctx());
        let r = fetch(proto, ports, "GET", "/page", &many, b"");
        assert_eq!(r.status, 431, "65 fields: {}", ctx());
        let r = fetch(proto, ports, "GET", &long_path, &[], b"");
        assert_eq!(r.status, 414, "9 KB path: {}", ctx());
        let r = fetch(proto, ports, "GET", "/small/x", &[("x-mid", &mid)], b"");
        assert_eq!(r.status, 431, "route limit: {}", ctx());
        if proto != Proto::H1 {
            let r = post_body_later(proto, ports, "/small/up", &[("x-mid", &mid)], b"x");
            assert_eq!(r, Ok(431), "route limit, body later: {}", ctx());
            let r = post_body_later(proto, ports, "/up", &[("x-big", &big)], b"x");
            assert_eq!(r, Ok(431), "10 KB header, body later: {}", ctx());
        }
        assert_eq!(
            seen_len(&seen),
            before,
            "a rejected request reached the upstream: {}",
            ctx()
        );
        // 上限の内側は通る（クライアントが足すヘッダの分を残した 60 フィールド・ルートの上限の無いルートの 2 KB）
        let r = fetch(proto, ports, "GET", "/page", &many[..60], b"");
        assert_eq!(r.status, 200, "60 fields: {}", ctx());
        let r = fetch(proto, ports, "GET", "/page", &[("x-mid", &mid)], b"");
        assert_eq!(r.status, 200, "2 KB header: {}", ctx());
    }
}

/// 上流が受けた要求の頭から、名前が `name` の行の値を全部返す
fn header_lines(head: &str, name: &str) -> Vec<String> {
    head.lines()
        .filter_map(|l| {
            let (n, v) = l.split_once(':')?;
            n.trim()
                .eq_ignore_ascii_case(name)
                .then(|| v.trim().to_string())
        })
        .collect()
}

/// B-489: HTTP/2・HTTP/3 で分けて送られた cookie（crumb）は `"; "` でつないだ 1 つの `cookie` として HTTP/1.1 の上流へ渡し、
/// 一貫性ハッシュの `cookie:` キーも 2 つ目以降のかけらから読む（以前は複数の `cookie` 行のまま渡し、ハッシュは
/// 1 つ目のかけらしか見なかった）
#[test]
fn b489_cookie_crumbs_are_joined() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    let (a, seen_a) = spawn_recording_upstream(|_| (200, String::new(), b"a".to_vec()));
    let (b, seen_b) = spawn_recording_upstream(|_| (200, String::new(), b"b".to_vec()));
    let ports = Ports::alloc();
    let config = format!(
        r#"{listeners}
[upstreams."ch"]
algorithm = "consistent_hash"
hash_key = "cookie:sid"
servers = ["http://127.0.0.1:{a}", "http://127.0.0.1:{b}"]

[[route]]
[route.conditions]
path = "/app/*"
[route.action]
type = "Proxy"
upstream = "ch"

[[route]]
[route.conditions]
path = "/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{a}"
"#,
        listeners = listeners_toml(ports, &cert, &key),
    );
    let veil = spawn_config(dir.path(), ports, &config);
    for proto in all_protos().into_iter().filter(|p| *p != Proto::H1) {
        let n0 = seen_len(&seen_a);
        let r = fetch(
            proto,
            ports,
            "GET",
            "/page",
            &[("cookie", "a=1"), ("cookie", "b=2")],
            b"",
        );
        assert_eq!(r.status, 200, "{proto:?}\n{}", veil.log());
        let head = seen_a.lock().unwrap()[n0].head.clone();
        assert_eq!(
            header_lines(&head, "cookie"),
            vec!["a=1; b=2".to_string()],
            "{proto:?}\n{head}"
        );
    }
    let mut used = std::collections::HashSet::new();
    for i in 0..8 {
        let sid = format!("sid=user{i}");
        let joined = format!("a=1; {sid}");
        let h1 = which_upstream(&seen_a, &seen_b, || {
            let r = fetch(
                Proto::H1,
                ports,
                "GET",
                "/app/x",
                &[("cookie", &joined)],
                b"",
            );
            assert_eq!(r.status, 200, "{}", veil.log());
        });
        used.insert(h1);
        for proto in all_protos().into_iter().filter(|p| *p != Proto::H1) {
            let got = which_upstream(&seen_a, &seen_b, || {
                let r = fetch(
                    proto,
                    ports,
                    "GET",
                    "/app/x",
                    &[("cookie", "a=1"), ("cookie", &sid)],
                    b"",
                );
                assert_eq!(r.status, 200, "{}", veil.log());
            });
            assert_eq!(got, h1, "{proto:?} {sid}\n{}", veil.log());
        }
    }
    assert_eq!(
        used.len(),
        2,
        "the cookies should spread over both upstreams"
    );
}

/// B-490: 拡張メソッド（WebDAV の PROPFIND）は HTTP/1.1 でも HTTP/2・HTTP/3 と同じく上流へ届き、`allowed_methods` で
/// 絞れる（以前の HTTP/1.1 は 9 つ以外のメソッドを 405 で断り、管理 API の PURGE も届かなかった）
#[test]
fn b490_extension_methods_on_every_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    let (up, seen) = spawn_recording_upstream(|_| (207, String::new(), b"dav".to_vec()));
    let ports = Ports::alloc();
    let config = format!(
        r#"{listeners}
[[route]]
[route.conditions]
path = "/ro/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up}"
[route.security]
allowed_methods = ["GET"]

[[route]]
[route.conditions]
path = "/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up}"
"#,
        listeners = listeners_toml(ports, &cert, &key),
    );
    let veil = spawn_config(dir.path(), ports, &config);
    for proto in all_protos() {
        let n0 = seen_len(&seen);
        let r = fetch(proto, ports, "PROPFIND", "/dav/x", &[("depth", "1")], b"");
        assert_eq!(r.status, 207, "{proto:?}\n{}", veil.log());
        let head = seen.lock().unwrap()[n0].head.clone();
        assert!(head.starts_with("PROPFIND /dav/x "), "{proto:?}\n{head}");
        let r = fetch(proto, ports, "PROPFIND", "/ro/x", &[], b"");
        assert_eq!(r.status, 405, "{proto:?}\n{}", veil.log());
        assert_eq!(seen_len(&seen), n0 + 1, "{proto:?}\n{}", veil.log());
    }
}

/// B-487: 応答を受け始めてから `pause` の間、フロー制御のクレジットを出さずに待ち（HTTP/2 は WINDOW_UPDATE を送らず、HTTP/3 は
/// 読まずに MAX_STREAM_DATA を出させない）、その後で本文を全部読む。読めた本文のバイト数。Err はストリームが打ち切られた
fn paused_download(
    proto: Proto,
    ports: Ports,
    path: &str,
    pause: Duration,
) -> Result<usize, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let path = path.to_string();
    rt.block_on(async move {
        let run = async {
            match proto {
                Proto::H2c | Proto::H2Tls => {
                    let tls = proto == Proto::H2Tls;
                    let port = if tls { ports.tls } else { ports.clear };
                    let mut client = h2_connect(tls, port).await?;
                    let req = h2_request(tls, port, "GET", &path, &[]);
                    let (resp, _send) =
                        client.send_request(req, true).map_err(|e| e.to_string())?;
                    let resp = resp.await.map_err(|e| e.to_string())?;
                    if resp.status() != 200 {
                        return Err(format!("status {}", resp.status()));
                    }
                    let mut body = resp.into_body();
                    tokio::time::sleep(pause).await;
                    let mut n = 0;
                    while let Some(c) = body.data().await {
                        let c = c.map_err(|e| e.to_string())?;
                        n += c.len();
                        let _ = body.flow_control().release_capacity(c.len());
                    }
                    Ok(n)
                }
                #[cfg(feature = "http3")]
                Proto::H3 => {
                    let (_endpoint, mut send) = h3_connect(ports.tls).await?;
                    let req = h3_request(ports.tls, "GET", &path, &[]);
                    let mut stream = send.send_request(req).await.map_err(|e| e.to_string())?;
                    stream.finish().await.map_err(|e| e.to_string())?;
                    let resp = stream.recv_response().await.map_err(|e| e.to_string())?;
                    if resp.status() != 200 {
                        return Err(format!("status {}", resp.status()));
                    }
                    tokio::time::sleep(pause).await;
                    let mut n = 0;
                    while let Some(mut c) = stream.recv_data().await.map_err(|e| e.to_string())? {
                        let len = bytes::Buf::remaining(&c);
                        n += len;
                        bytes::Buf::advance(&mut c, len);
                    }
                    Ok(n)
                }
                _ => unreachable!(),
            }
        };
        tokio::time::timeout(pause + Duration::from_secs(20), run)
            .await
            .unwrap_or_else(|_| Err("client timed out".to_string()))
    })
}

/// B-487: フロー制御のクレジットを出さないクライアントへの応答は、`buffering = "full"` のルートでは
/// `client_write_timeout_secs` の後に HTTP/2・HTTP/3 のストリームをリセットする（HTTP/1.1 と同じ）。ほかのルートは既定の
/// `WRITE_TIMEOUT`（30 秒）まで待つので、数秒止めただけなら最後まで届く（以前の HTTP/2・HTTP/3 はいつまでも待った）
#[test]
fn b487_buffered_response_stalled_by_flow_control_is_reset() {
    const BODY: usize = 4 << 20;
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    let (up, _seen) = spawn_recording_upstream(|_| (200, String::new(), vec![b'x'; BODY]));
    let ports = Ports::alloc();
    let config = format!(
        r#"{listeners}
[[route]]
[route.conditions]
path = "/full/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up}"
[route.buffering]
mode = "full"
client_write_timeout_secs = 2

[[route]]
[route.conditions]
path = "/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up}"
"#,
        listeners = listeners_toml(ports, &cert, &key),
    );
    let veil = spawn_config(dir.path(), ports, &config);
    // 見回りは 1 秒ごとで、2 秒の設定なら止まってから 3〜4 秒で打ち切る
    let pause = Duration::from_secs(8);
    let protos: Vec<Proto> = all_protos()
        .into_iter()
        .filter(|p| *p != Proto::H1)
        .collect();
    let results: Vec<(Proto, &str, Result<usize, String>)> = std::thread::scope(|s| {
        let handles: Vec<_> = protos
            .iter()
            .flat_map(|&proto| {
                ["/full/x", "/stream/x"].map(|path| {
                    s.spawn(move || (proto, path, paused_download(proto, ports, path, pause)))
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for (proto, path, r) in results {
        let ctx = format!("{proto:?} {path}: {r:?}\n{}", veil.log());
        if path.starts_with("/full") {
            assert!(matches!(r, Err(ref e) if !e.contains("timed out")), "{ctx}");
        } else {
            assert_eq!(r, Ok(BODY), "{ctx}");
        }
    }
}

/// `Sec-WebSocket-Key` に対する `Sec-WebSocket-Accept`（RFC 6455 §4.2.2）
fn ws_accept(key: &str) -> String {
    use base64::Engine;
    let mut data = key.as_bytes().to_vec();
    data.extend_from_slice(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    let d = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, &data);
    base64::engine::general_purpose::STANDARD.encode(d.as_ref())
}

/// B-487: 本文を `1 GiB` と名乗って書けるだけ書く HTTP/1.1 の上流（WebSocket の upgrade には 101 を返してから書き続ける）。
/// 要求の行と、書き込みが失敗した（veil が上流の接続を閉じた）時刻を記録する
fn spawn_endless_upstream() -> (u16, Arc<Mutex<Vec<(String, std::time::Instant)>>>) {
    use std::io::{Read, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let closed = Arc::new(Mutex::new(Vec::new()));
    let closed2 = closed.clone();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            let closed = closed2.clone();
            std::thread::spawn(move || {
                let mut head = Vec::new();
                let mut b = [0u8; 4096];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut b) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => head.extend_from_slice(&b[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let line = head.lines().next().unwrap_or_default().to_string();
                let ws_key = head.lines().find_map(|l| {
                    let (n, v) = l.split_once(':')?;
                    n.trim()
                        .eq_ignore_ascii_case("sec-websocket-key")
                        .then(|| v.trim().to_string())
                });
                let resp_head = match ws_key {
                    Some(key) => format!(
                        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
                         Sec-WebSocket-Accept: {}\r\n\r\n",
                        ws_accept(&key)
                    ),
                    None => format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", 1u64 << 30),
                };
                let chunk = vec![b'x'; 65536];
                let ok = s.write_all(resp_head.as_bytes()).is_ok()
                    && (0..(1usize << 30) / chunk.len()).all(|_| s.write_all(&chunk).is_ok());
                if !ok {
                    closed
                        .lock()
                        .unwrap()
                        .push((line, std::time::Instant::now()));
                }
            });
        }
    });
    (port, closed)
}

/// B-487: TCP の受信を止めたクライアント（h2c を直に話す）を `stop` まで続ける。WINDOW_UPDATE で窓を最大にしてから要求を送り、
/// 以後は何も読まない（サーバの送信がソケットで詰まる）
fn h2c_stop_reading(port: u16, path: &str, stop: &std::sync::atomic::AtomicBool) {
    use std::io::Write;
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n").unwrap();
    // SETTINGS（INITIAL_WINDOW_SIZE = 2^31-1）
    s.write_all(&[0, 0, 6, 0x4, 0, 0, 0, 0, 0, 0, 0x4, 0x7f, 0xff, 0xff, 0xff])
        .unwrap();
    // WINDOW_UPDATE（接続。2^31-1 まで広げる）
    let inc: u32 = 0x7fff_ffff - 65535;
    let mut wu = vec![0, 0, 4, 0x8, 0, 0, 0, 0, 0];
    wu.extend_from_slice(&inc.to_be_bytes());
    s.write_all(&wu).unwrap();
    // HEADERS（stream 1、END_STREAM | END_HEADERS）: GET（静的表 2）・http（6）・:path・:authority（リテラル）
    let authority = format!("127.0.0.1:{port}");
    let mut block = vec![0x82, 0x86, 0x44, path.len() as u8];
    block.extend_from_slice(path.as_bytes());
    block.extend_from_slice(&[0x41, authority.len() as u8]);
    block.extend_from_slice(authority.as_bytes());
    let mut frame = vec![0, 0, block.len() as u8, 0x1, 0x5, 0, 0, 0, 1];
    frame.extend_from_slice(&block);
    s.write_all(&frame).unwrap();
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// B-487: 応答の見出しを受けた後、フロー制御のクレジットを出さずに `stop` まで待つ（ストリームは開いたまま）。
/// `tunnel` は HTTP/2 の拡張 CONNECT（WebSocket。上流が送り続ける）。HTTP/3 は PING を送り続けて QUIC のアイドルの
/// 時間切れを避ける
fn stall_after_head(
    proto: Proto,
    tunnel: bool,
    ports: Ports,
    path: &str,
    stop: &std::sync::atomic::AtomicBool,
) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let wait = || async {
        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    };
    rt.block_on(async {
        match proto {
            Proto::H2c => {
                let client = h2_connect(false, ports.clear).await.unwrap();
                let mut client = client.ready().await.unwrap();
                let req = if tunnel {
                    // h2 は SETTINGS_ENABLE_CONNECT_PROTOCOL を受けるまで `:protocol` を送れない
                    for _ in 0..100 {
                        if client.is_extended_connect_protocol_enabled() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        client = client.ready().await.unwrap();
                    }
                    http::Request::builder()
                        .method(http::Method::CONNECT)
                        .uri(format!("http://localhost:{}{path}", ports.clear))
                        .extension(h2::ext::Protocol::from_static("websocket"))
                        .header("sec-websocket-version", "13")
                        .body(())
                        .unwrap()
                } else {
                    h2_request(false, ports.clear, "GET", path, &[])
                };
                // トンネルは要求の側も開いたまま（END_STREAM を送らない）
                let (resp, _send) = client.send_request(req, !tunnel).unwrap();
                let resp = resp.await.unwrap();
                assert_eq!(resp.status(), 200);
                let _body = resp.into_body();
                wait().await;
            }
            #[cfg(feature = "http3")]
            Proto::H3 => {
                let mut transport = quinn::TransportConfig::default();
                transport.keep_alive_interval(Some(Duration::from_secs(2)));
                let (_endpoint, mut send) = h3_connect_with(ports.tls, transport).await.unwrap();
                let req = h3_request(ports.tls, "GET", path, &[]);
                let mut stream = send.send_request(req).await.unwrap();
                stream.finish().await.unwrap();
                let resp = stream.recv_response().await.unwrap();
                assert_eq!(resp.status(), 200);
                wait().await;
            }
            _ => unreachable!(),
        }
    });
}

/// B-487: TCP の受信を止めた HTTP/2 の接続は `WRITE_TIMEOUT`（30 秒）〜その 2 倍で閉じ、フロー制御のクレジットを出さない
/// HTTP/2・HTTP/3 のストリーム（ストリーミングの中継・HTTP/2 の WebSocket のトンネル）は `WRITE_TIMEOUT` の後にリセットされる。
/// どれも中継していた上流の接続を放す（以前はいつまでも握った。HTTP/1.1 は書き込みの時間切れで閉じる。HTTP/3 は PING で
/// QUIC のアイドルの時間切れを避けるクライアントで確かめる）
#[test]
fn b487_stalled_clients_release_upstream_after_write_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    let (up, closed) = spawn_endless_upstream();
    let ports = Ports::alloc();
    let config = format!(
        r#"{listeners}
[http2]
enable_connect_protocol = true

[[route]]
[route.conditions]
path = "/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up}"
"#,
        listeners = listeners_toml(ports, &cert, &key),
    );
    let veil = spawn_config(dir.path(), ports, &config);
    let stop = std::sync::atomic::AtomicBool::new(false);
    let started = std::time::Instant::now();
    #[allow(unused_mut)]
    let mut expect: Vec<(&str, Duration, Duration)> = vec![
        // 書き込みの見張りは張り直しを含めて 30〜60 秒
        (
            "/b487/tcp",
            Duration::from_secs(28),
            Duration::from_secs(75),
        ),
        // 見回りは 1 秒ごと（30 秒を超えてから打ち切る）
        ("/b487/h2", Duration::from_secs(28), Duration::from_secs(45)),
        ("/b487/ws", Duration::from_secs(28), Duration::from_secs(45)),
    ];
    #[cfg(feature = "http3")]
    expect.push(("/b487/h3", Duration::from_secs(28), Duration::from_secs(45)));
    let all_closed = std::thread::scope(|s| {
        s.spawn(|| h2c_stop_reading(ports.clear, "/b487/tcp", &stop));
        s.spawn(|| stall_after_head(Proto::H2c, false, ports, "/b487/h2", &stop));
        s.spawn(|| stall_after_head(Proto::H2c, true, ports, "/b487/ws", &stop));
        #[cfg(feature = "http3")]
        s.spawn(|| stall_after_head(Proto::H3, false, ports, "/b487/h3", &stop));
        let ok = wait_until(Duration::from_secs(80), || {
            closed.lock().unwrap().len() >= expect.len()
        });
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        ok
    });
    let closed = closed.lock().unwrap().clone();
    let ctx = format!("{closed:?} (started {started:?})\n{}", veil.log());
    assert!(all_closed, "upstream connections were kept: {ctx}");
    for (path, min, max) in expect {
        let at = closed
            .iter()
            .find(|(line, _)| line.starts_with(&format!("GET {path} ")))
            .map(|(_, t)| t.duration_since(started))
            .unwrap_or_else(|| panic!("{path}: upstream kept\n{ctx}"));
        assert!(
            at >= min && at <= max,
            "{path}: released after {at:?}\n{ctx}"
        );
    }
}

/// B-487: `n` 本の GET を 1 つの接続で同時に送り、全部の本文を `rate`（バイト毎秒）でゆっくり読む。接続のウィンドウは
/// 64 KB、ストリームのウィンドウは 4 MB（空いた接続のクレジットを veil の順番で先のストリームが使い続け、後のストリームは
/// 接続のクレジットを待つ）。ストリームごとに読めた本文のバイト数（打ち切られたら Err）
fn slow_parallel_download(
    proto: Proto,
    ports: Ports,
    path: &str,
    n: usize,
    rate: u64,
) -> Vec<Result<usize, String>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let path = path.to_string();
    let pace = move |len: usize| Duration::from_micros(len as u64 * 1_000_000 / rate);
    rt.block_on(async move {
        let mut tasks = Vec::new();
        match proto {
            Proto::H2c => {
                let tcp = tokio::net::TcpStream::connect(("127.0.0.1", ports.clear))
                    .await
                    .unwrap();
                let (client, conn) = h2::client::Builder::new()
                    .initial_window_size(4 << 20)
                    .initial_connection_window_size(65535)
                    .handshake::<_, bytes::Bytes>(tcp)
                    .await
                    .unwrap();
                tokio::spawn(async move {
                    let _ = conn.await;
                });
                for _ in 0..n {
                    let mut client = client.clone().ready().await.unwrap();
                    let req = h2_request(false, ports.clear, "GET", &path, &[]);
                    let (resp, _send) = client.send_request(req, true).unwrap();
                    tasks.push(tokio::spawn(async move {
                        let resp = resp.await.map_err(|e| e.to_string())?;
                        let mut body = resp.into_body();
                        let mut got = 0;
                        while let Some(c) = body.data().await {
                            let c = c.map_err(|e| e.to_string())?;
                            got += c.len();
                            tokio::time::sleep(pace(c.len())).await;
                            let _ = body.flow_control().release_capacity(c.len());
                        }
                        Ok(got)
                    }));
                }
            }
            #[cfg(feature = "http3")]
            Proto::H3 => {
                let mut transport = quinn::TransportConfig::default();
                transport.receive_window(quinn::VarInt::from_u32(65536));
                transport.stream_receive_window(quinn::VarInt::from_u32(4 << 20));
                let (endpoint, send) = h3_connect_with(ports.tls, transport).await.unwrap();
                for _ in 0..n {
                    let mut send = send.clone();
                    let req = h3_request(ports.tls, "GET", &path, &[]);
                    let mut stream = send.send_request(req).await.unwrap();
                    stream.finish().await.unwrap();
                    tasks.push(tokio::spawn(async move {
                        stream.recv_response().await.map_err(|e| e.to_string())?;
                        let mut got = 0;
                        while let Some(mut c) =
                            stream.recv_data().await.map_err(|e| e.to_string())?
                        {
                            let len = bytes::Buf::remaining(&c);
                            got += len;
                            bytes::Buf::advance(&mut c, len);
                            tokio::time::sleep(pace(len)).await;
                        }
                        Ok(got)
                    }));
                }
                let mut out = Vec::new();
                for t in tasks {
                    out.push(t.await.unwrap());
                }
                drop(endpoint);
                return out;
            }
            _ => unreachable!(),
        }
        let mut out = Vec::new();
        for t in tasks {
            out.push(t.await.unwrap());
        }
        out
    })
}

/// B-487: 接続のウィンドウだけが小さく、全部のストリームをゆっくり読むクライアント（遅い回線で並行に取る）は、空いた
/// 接続のクレジットを先のストリームが使い続けても、後のストリームを打ち切らない（自分のウィンドウは残っていて、接続は
/// 進んでいる。ストリームごとの進みだけを見ると、`client_write_timeout_secs` の 2 秒の間待たされたストリームを打ち切る）
#[test]
fn b487_slow_reader_of_parallel_streams_is_not_reset() {
    // 1 本を読むのに 3 秒（最後のストリームは 6 秒ほど待つ）
    const BODY: usize = 1200 * 1024;
    const RATE: u64 = 400 * 1024;
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    let (up, _seen) = spawn_recording_upstream(|_| (200, String::new(), vec![b'x'; BODY]));
    let ports = Ports::alloc();
    let config = format!(
        r#"{listeners}
[[route]]
[route.conditions]
path = "/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up}"
[route.buffering]
mode = "full"
client_write_timeout_secs = 2
"#,
        listeners = listeners_toml(ports, &cert, &key),
    );
    let veil = spawn_config(dir.path(), ports, &config);
    #[allow(unused_mut)]
    let mut protos = vec![Proto::H2c];
    #[cfg(feature = "http3")]
    protos.push(Proto::H3);
    let results: Vec<(Proto, Vec<Result<usize, String>>)> = std::thread::scope(|s| {
        let handles: Vec<_> = protos
            .iter()
            .map(|&proto| {
                s.spawn(move || (proto, slow_parallel_download(proto, ports, "/x", 3, RATE)))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for (proto, r) in results {
        assert_eq!(r, vec![Ok(BODY); 3], "{proto:?}\n{}", veil.log());
    }
}

/// gRPC の 1 メッセージ（長さ 0）
#[cfg(all(feature = "wasm", feature = "grpc"))]
const GRPC_EMPTY_MESSAGE: &[u8] = &[0, 0, 0, 0, 0];

/// gRPC の応答を返す h2c の上流: HEADERS（200・`application/grpc`）→ DATA（1 メッセージ）→ トレーラ
/// （`grpc-status: 5`・`grpc-message: original`）。要求の本文は読み捨てる
#[cfg(all(feature = "wasm", feature = "grpc"))]
fn spawn_grpc_trailers_upstream() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.set_nonblocking(true).unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let l = tokio::net::TcpListener::from_std(l).unwrap();
            loop {
                let Ok((tcp, _)) = l.accept().await else {
                    continue;
                };
                tokio::spawn(async move {
                    let Ok(mut conn) = h2::server::handshake(tcp).await else {
                        return;
                    };
                    while let Some(Ok((req, mut respond))) = conn.accept().await {
                        tokio::spawn(async move {
                            let mut body = req.into_body();
                            while let Some(Ok(c)) = body.data().await {
                                let _ = body.flow_control().release_capacity(c.len());
                            }
                            let resp = http::Response::builder()
                                .status(200)
                                .header("content-type", "application/grpc")
                                .body(())
                                .unwrap();
                            let Ok(mut send) = respond.send_response(resp, false) else {
                                return;
                            };
                            let _ = send
                                .send_data(bytes::Bytes::from_static(GRPC_EMPTY_MESSAGE), false);
                            let mut trailers = http::HeaderMap::new();
                            trailers.insert("grpc-status", "5".parse().unwrap());
                            trailers.insert("grpc-message", "original".parse().unwrap());
                            let _ = send.send_trailers(trailers);
                        });
                    }
                });
            }
        });
    });
    port
}

#[cfg(all(feature = "wasm", feature = "grpc"))]
fn header_map_pairs(h: &http::HeaderMap) -> Vec<(String, String)> {
    h.iter()
        .map(|(n, v)| {
            (
                n.as_str().to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect()
}

/// gRPC の呼び出しの結果（ステータス・応答のヘッダ・応答のトレーラ）
#[cfg(all(feature = "wasm", feature = "grpc"))]
type GrpcOutcome = (u16, Vec<(String, String)>, Vec<(String, String)>);

/// `path` へ gRPC の呼び出し（本文 1 メッセージ）を送る。`trailers` があれば本文の後に `delay` だけ待って要求の
/// トレーラとして送る
#[cfg(all(feature = "wasm", feature = "grpc"))]
fn grpc_call(
    proto: Proto,
    ports: Ports,
    path: &str,
    trailers: &[(&str, &str)],
    delay: Duration,
) -> Result<GrpcOutcome, String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut map = http::HeaderMap::new();
    for (n, v) in trailers {
        map.insert(
            http::HeaderName::from_bytes(n.as_bytes()).unwrap(),
            v.parse().unwrap(),
        );
    }
    let path = path.to_string();
    let call = async move {
        let headers = [("content-type", "application/grpc"), ("te", "trailers")];
        match proto {
            Proto::H2c | Proto::H2Tls => {
                let tls = proto == Proto::H2Tls;
                let port = if tls { ports.tls } else { ports.clear };
                let mut client = h2_connect(tls, port).await?;
                let owned: Vec<(String, String)> = headers
                    .iter()
                    .map(|(n, v)| (n.to_string(), v.to_string()))
                    .collect();
                let req = h2_request(tls, port, "POST", &path, &owned);
                let (resp, mut send) =
                    client.send_request(req, false).map_err(|e| e.to_string())?;
                send.send_data(
                    bytes::Bytes::from_static(GRPC_EMPTY_MESSAGE),
                    map.is_empty(),
                )
                .map_err(|e| e.to_string())?;
                if !map.is_empty() {
                    tokio::time::sleep(delay).await;
                    send.send_trailers(map).map_err(|e| e.to_string())?;
                }
                let resp = resp.await.map_err(|e| e.to_string())?;
                let status = resp.status().as_u16();
                let resp_headers = header_map_pairs(resp.headers());
                let mut body = resp.into_body();
                while let Some(c) = body.data().await {
                    let c = c.map_err(|e| e.to_string())?;
                    let _ = body.flow_control().release_capacity(c.len());
                }
                let t = body.trailers().await.map_err(|e| e.to_string())?;
                Ok((
                    status,
                    resp_headers,
                    t.as_ref().map(header_map_pairs).unwrap_or_default(),
                ))
            }
            #[cfg(feature = "http3")]
            Proto::H3 => {
                use bytes::Buf;
                let (_endpoint, mut send) = h3_connect(ports.tls).await?;
                let req = h3_request(ports.tls, "POST", &path, &headers);
                let mut stream = send.send_request(req).await.map_err(|e| e.to_string())?;
                stream
                    .send_data(bytes::Bytes::from_static(GRPC_EMPTY_MESSAGE))
                    .await
                    .map_err(|e| e.to_string())?;
                if !map.is_empty() {
                    tokio::time::sleep(delay).await;
                    stream.send_trailers(map).await.map_err(|e| e.to_string())?;
                }
                stream.finish().await.map_err(|e| e.to_string())?;
                let resp = stream.recv_response().await.map_err(|e| e.to_string())?;
                let status = resp.status().as_u16();
                let resp_headers = header_map_pairs(resp.headers());
                while let Some(mut chunk) = stream.recv_data().await.map_err(|e| e.to_string())? {
                    let n = chunk.remaining();
                    chunk.advance(n);
                }
                let t = stream.recv_trailers().await.map_err(|e| e.to_string())?;
                Ok((
                    status,
                    resp_headers,
                    t.as_ref().map(header_map_pairs).unwrap_or_default(),
                ))
            }
            _ => Err(format!("gRPC is not tested over {proto:?}")),
        }
    };
    rt.block_on(async move {
        tokio::time::timeout(Duration::from_secs(20), call)
            .await
            .map_err(|_| "timed out".to_string())?
    })
}

/// F-313: Proxy-Wasm のトレーラの段が HTTP/2（h2c・TLS）と HTTP/3 で同じように走る。上流（h2c の gRPC）の応答の
/// トレーラを `on_http_response_trailers` が書き換え（`grpc-status`・`grpc-message`）、要求のトレーラ（`x-wasm-reject: 1`）を
/// 見た `on_http_request_trailers` のローカル応答（403）で要求を断る。HTTP/3 はトレーラが見出しと同じ束で届いても、
/// 分類の後に届いても渡す（以前の HTTP/3 はどちらの段も走らず、上流の `grpc-status: 5` と 200 が届いた）
#[cfg(all(feature = "wasm", feature = "grpc"))]
#[test]
fn f313_wasm_trailer_phases_on_http2_and_http3() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    // 生成される .cwasm を作業ツリーに残さないよう一時ディレクトリへ写す
    let wasm = dir.path().join("grpc_trailer_filter.wasm");
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/wasm/grpc_trailer_filter.wasm"),
        &wasm,
    )
    .unwrap();
    let up = spawn_grpc_trailers_upstream();
    let ports = Ports::alloc();
    let config = format!(
        r#"{listeners}
[wasm]
enabled = true

[[wasm.modules]]
name = "grpc_trailer_filter"
path = "{wasm}"
[wasm.modules.capabilities]
allow_logging = true
allow_send_local_response = true
allow_request_headers_read = true
allow_request_headers_write = true
allow_response_headers_read = true
allow_response_headers_write = true

[[route]]
modules = ["grpc_trailer_filter"]
[route.conditions]
path = "/grpc.test.v1.TestService/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up}"
use_h2c = true
"#,
        listeners = listeners_toml(ports, &cert, &key),
        wasm = tp(&wasm),
    );
    let veil = spawn_config(dir.path(), ports, &config);
    let path = "/grpc.test.v1.TestService/UnaryCall";
    let protos: Vec<Proto> = all_protos()
        .into_iter()
        .filter(|p| *p != Proto::H1)
        .collect();
    let trailer = |t: &[(String, String)], name: &str| {
        t.iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    for proto in protos {
        // 応答のトレーラの段（要求のトレーラ無し・断らないトレーラ付き）
        for sent in [&[][..], &[("x-other", "1")][..]] {
            let r = grpc_call(proto, ports, path, sent, Duration::ZERO);
            let ctx = format!("{proto:?} {sent:?}: {r:?}\n{}", veil.log());
            let (status, _, t) = r.expect(&ctx);
            assert_eq!(status, 200, "{ctx}");
            assert_eq!(trailer(&t, "grpc-status").as_deref(), Some("0"), "{ctx}");
            assert_eq!(
                trailer(&t, "grpc-message").as_deref(),
                Some("rewritten-by-wasm"),
                "{ctx}"
            );
        }
        // 要求のトレーラの段（すぐ送る＝見出しと同じ束・少し後に送る＝分類の後）
        for delay in [Duration::ZERO, Duration::from_millis(300)] {
            let r = grpc_call(proto, ports, path, &[("x-wasm-reject", "1")], delay);
            let ctx = format!("{proto:?} reject after {delay:?}: {r:?}\n{}", veil.log());
            let (status, headers, _) = r.expect(&ctx);
            assert_eq!(status, 403, "{ctx}");
            assert_eq!(
                trailer(&headers, "x-wasm-trailer-reject").as_deref(),
                Some("1"),
                "{ctx}"
            );
        }
    }
}
