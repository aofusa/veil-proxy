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
