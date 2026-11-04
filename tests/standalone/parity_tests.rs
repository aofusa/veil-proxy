//! プロトコル間で同じ機能が同じに効くことの回帰試験（B-266〜B-277・F-285〜F-287）。
//!
//! 同じ設定の veil に HTTP/1.1（平文）・h2c・HTTP/2（TLS）・HTTP/3 で同じ要求を送り、応答を比べる。
//! 以前は静的配信の HEAD・Range・条件付き要求、応答ヘッダの方針、Alt-Svc、h2c の上流の圧縮、
//! サーキットブレーカー、本文の受信の打ち切り、アップロードの途中のリセット、Wasm の応答本文の段が
//! プロトコルごとに違っていた。

use super::standalone::parity::*;
use super::standalone::*;
use std::time::{Duration, Instant};

fn text_body(len: usize) -> Vec<u8> {
    (0..len).map(|i| b'a' + (i % 26) as u8).collect()
}

fn gunzip(data: &[u8]) -> Vec<u8> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(data)
        .read_to_end(&mut out)
        .expect("gzip body");
    out
}

/// B-266・B-269・F-286・F-287: 静的配信（`type = "File"` の sendfile・memory）の HEAD・Range・416・
/// 条件付き要求（304・If-Range）・検証子・圧縮が 4 つのプロトコルで同じになる
#[test]
fn b266_f286_f287_static_responses_match_across_protocols() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    let www = dir.path().join("www");
    std::fs::create_dir_all(&www).unwrap();
    let content = text_body(5000);
    std::fs::write(www.join("a.txt"), &content).unwrap();
    let ports = Ports::alloc();
    let compression = r#"
[route.compression]
enabled = true
preferred_encodings = ["gzip"]
min_size = 100
"#;
    let config = format!(
        r#"{listeners}
[[route]]
[route.conditions]
path = "/static/*"
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
path = "/gz/*"
[route.action]
type = "File"
path = "{www}"
mode = "sendfile"
{compression}

[[route]]
[route.conditions]
path = "/gzmem"
[route.action]
type = "File"
path = "{www}/a.txt"
mode = "memory"
{compression}
"#,
        listeners = listeners_toml(ports, &cert, &key),
        www = tp(&www),
    );
    let veil = spawn_config(dir.path(), ports, &config);
    let len = content.len();

    for proto in all_protos() {
        for path in ["/static/a.txt", "/mem"] {
            let ctx = format!("{proto:?} {path}\n{}", veil.log());
            let get = |headers: &[(&str, &str)]| fetch(proto, ports, "GET", path, headers, b"");

            let full = get(&[]);
            assert_eq!(full.status, 200, "{ctx}");
            assert_eq!(full.body, content, "{ctx}");
            assert_eq!(full.header("accept-ranges"), Some("bytes"), "{ctx}");
            let etag = full.header("etag").expect("etag").to_string();
            let last_modified = full
                .header("last-modified")
                .expect("last-modified")
                .to_string();
            assert!(etag.starts_with('"') && etag.ends_with('"'), "{etag} {ctx}");

            // HEAD は本文を送らない（以前は本文が付き、HTTP/1.1 は次の応答がずれ、HTTP/2 は PROTOCOL_ERROR）
            let head = fetch(proto, ports, "HEAD", path, &[], b"");
            assert_eq!(head.status, 200, "{ctx}");
            assert!(
                head.body.is_empty(),
                "HEAD body {:?} {ctx}",
                head.body.len()
            );
            if let Some(cl) = head.header("content-length") {
                assert_eq!(cl, len.to_string(), "{ctx}");
            }
            assert_eq!(head.header("etag"), Some(etag.as_str()), "{ctx}");

            // Range
            let r = get(&[("range", "bytes=0-9")]);
            assert_eq!(r.status, 206, "{ctx}");
            assert_eq!(r.body, content[..10], "{ctx}");
            assert_eq!(
                r.header("content-range"),
                Some(format!("bytes 0-9/{len}").as_str()),
                "{ctx}"
            );
            let r = get(&[("range", "bytes=-10")]);
            assert_eq!(r.status, 206, "{ctx}");
            assert_eq!(r.body, content[len - 10..], "{ctx}");
            let r = get(&[("range", "bytes=10-")]);
            assert_eq!(r.status, 206, "{ctx}");
            assert_eq!(r.body, content[10..], "{ctx}");
            let r = get(&[("range", "bytes=99999-")]);
            assert_eq!(r.status, 416, "{ctx}");
            assert_eq!(
                r.header("content-range"),
                Some(format!("bytes */{len}").as_str()),
                "{ctx}"
            );

            // 条件付き要求
            let r = get(&[("if-none-match", etag.as_str())]);
            assert_eq!(r.status, 304, "{ctx}");
            assert!(r.body.is_empty(), "{ctx}");
            let r = get(&[("if-modified-since", last_modified.as_str())]);
            assert_eq!(r.status, 304, "{ctx}");
            let r = get(&[("if-none-match", "\"other\"")]);
            assert_eq!(r.status, 200, "{ctx}");

            // If-Range: 一致すれば範囲、違えば全体
            let r = get(&[("range", "bytes=0-9"), ("if-range", etag.as_str())]);
            assert_eq!(r.status, 206, "{ctx}");
            let r = get(&[("range", "bytes=0-9"), ("if-range", "\"other\"")]);
            assert_eq!(r.status, 200, "{ctx}");
            assert_eq!(r.body.len(), len, "{ctx}");
        }

        // 圧縮（B-269: HTTP/1.1 の静的配信は圧縮しなかった）
        for path in ["/gz/a.txt", "/gzmem"] {
            let ctx = format!("{proto:?} {path}\n{}", veil.log());
            let r = fetch(
                proto,
                ports,
                "GET",
                path,
                &[("accept-encoding", "gzip")],
                b"",
            );
            assert_eq!(r.status, 200, "{ctx}");
            assert_eq!(r.header("content-encoding"), Some("gzip"), "{ctx}");
            assert_eq!(r.header("vary"), Some("Accept-Encoding"), "{ctx}");
            assert!(
                r.header("etag").is_some_and(|e| e.starts_with("W/")),
                "{ctx}"
            );
            assert_eq!(gunzip(&r.body), content, "{ctx}");
            let h = fetch(
                proto,
                ports,
                "HEAD",
                path,
                &[("accept-encoding", "gzip")],
                b"",
            );
            assert_eq!(h.status, 200, "{ctx}");
            assert!(h.body.is_empty(), "{ctx}");
            // 圧縮しない要求には Range が効く
            let r = fetch(proto, ports, "GET", path, &[("range", "bytes=0-9")], b"");
            assert_eq!(r.status, 206, "{ctx}");
        }
    }
}

/// B-266・B-267・B-271: 応答ヘッダの方針（`add_response_headers`・`remove_response_headers`）がプロキシ
/// （ストリーミング・バッファ）と固定応答のどれにも 4 つのプロトコルで効く。Alt-Svc は HTTP/1.1・HTTP/2 の
/// 応答に付き、`server` は `server_header_enabled` のときだけ全プロトコルで同じ値になる。固定応答とエラー応答の
/// HEAD は本文を送らない
#[test]
fn b267_b271_header_policy_alt_svc_and_server_header_match_across_protocols() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    let (up, _seen) = spawn_recording_upstream(|_| {
        (
            200,
            "Server: upstream\r\nX-Up: 1\r\nContent-Type: text/plain\r\n".to_string(),
            b"hello".to_vec(),
        )
    });
    let ports = Ports::alloc();
    let policy = r#"
[route.security]
add_response_headers = { "X-Added" = "1" }
remove_response_headers = ["X-Up"]
"#;
    let config = format!(
        r#"{listeners}
[[route]]
[route.conditions]
path = "/p"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up}/p"
{policy}

[[route]]
[route.conditions]
path = "/pbuf"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up}/p"
[route.buffering]
mode = "full"
{policy}

[[route]]
[route.conditions]
path = "/r"
[route.action]
type = "Respond"
status = 200
body = "respond"
{policy}

[[route]]
[route.conditions]
path = "/hide"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up}/p"
{hide}

[[route]]
[route.conditions]
path = "/hide-buf"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up}/p"
[route.buffering]
mode = "full"
{hide}

[[route]]
[route.conditions]
path = "/hide-r"
[route.action]
type = "Respond"
status = 200
body = "respond"
{hide}
"#,
        hide = "[route.security]\nremove_response_headers = [\"Server\", \"Alt-Svc\"]\n",
        listeners = listeners_toml(ports, &cert, &key).replace(
            "threads = 1\n",
            "threads = 1\nserver_header_enabled = true\n"
        ),
    );
    let veil = spawn_config(dir.path(), ports, &config);

    for proto in all_protos() {
        for path in ["/p", "/pbuf", "/r"] {
            let ctx = format!("{proto:?} {path}\n{}", veil.log());
            let r = fetch(proto, ports, "GET", path, &[], b"");
            assert_eq!(r.status, 200, "{ctx}");
            assert_eq!(r.header("x-added"), Some("1"), "{ctx}");
            assert_eq!(r.header("x-up"), None, "{ctx}");
            if proto == Proto::H3 {
                assert_eq!(r.header("alt-svc"), None, "{ctx}");
            } else {
                assert!(
                    r.header("alt-svc").is_some_and(|v| v.contains("h3=")),
                    "{:?} {ctx}",
                    r.headers
                );
            }
            if path == "/r" {
                assert_eq!(r.header("server"), Some("veil"), "{ctx}");
                assert_eq!(r.body, b"respond", "{ctx}");
                let h = fetch(proto, ports, "HEAD", path, &[], b"");
                assert_eq!(h.status, 200, "{ctx}");
                assert!(h.body.is_empty(), "{ctx}");
            }
        }
        // `remove_response_headers` の `Server`・`Alt-Svc` は自分の `server`・HTTP/3 の広告も消す
        for path in ["/hide", "/hide-buf", "/hide-r"] {
            let r = fetch(proto, ports, "GET", path, &[], b"");
            let ctx = format!("{proto:?} {path} {:?}", r.headers);
            assert_eq!(r.status, 200, "{ctx}");
            assert_eq!(r.header("server"), None, "{ctx}");
            assert_eq!(r.header("alt-svc"), None, "{ctx}");
        }
        // ルートの無い要求のエラー応答も HEAD には本文を付けない
        let h = fetch(proto, ports, "HEAD", "/missing", &[], b"");
        assert_eq!(h.status, 404, "{proto:?}");
        assert!(h.body.is_empty(), "{proto:?}");
    }
}

/// B-270: h2c の上流の応答にも `[route.compression]` が全プロトコルで効く
/// （上流は同じ veil の memory の静的配信を h2c で引く）。B-279: `buffering = "full"` の上流の応答を
/// 圧縮したら Content-Encoding を付ける（HTTP/3 は付けずに圧縮した本文を返していた）
#[test]
fn b270_h2c_upstream_responses_are_compressed() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    let www = dir.path().join("www");
    std::fs::create_dir_all(&www).unwrap();
    let content = text_body(4800);
    std::fs::write(www.join("a.txt"), &content).unwrap();
    let ports = Ports::alloc();
    let config = format!(
        r#"{listeners}
[prometheus]
enabled = true

[[route]]
[route.conditions]
path = "/origin"
[route.action]
type = "File"
path = "{www}/a.txt"
mode = "memory"

[[route]]
[route.conditions]
path = "/front"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{clear}/origin"
use_h2c = true
[route.compression]
enabled = true
preferred_encodings = ["gzip"]
min_size = 100

[[route]]
[route.conditions]
path = "/front-buffered"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{clear}/origin"
[route.buffering]
mode = "full"
[route.compression]
enabled = true
preferred_encodings = ["gzip"]
min_size = 100
"#,
        listeners = listeners_toml(ports, &cert, &key),
        www = tp(&www),
        clear = ports.clear,
    );
    let veil = spawn_config(dir.path(), ports, &config);
    let protos = all_protos();
    for proto in protos.iter().copied() {
        for path in ["/front", "/front-buffered"] {
            let r = fetch(
                proto,
                ports,
                "GET",
                path,
                &[("accept-encoding", "gzip")],
                b"",
            );
            let ctx = format!("{proto:?} {path} {:?}\n{}", r.headers, veil.log());
            assert_eq!(r.status, 200, "{ctx}");
            assert_eq!(r.header("content-encoding"), Some("gzip"), "{ctx}");
            assert_eq!(gunzip(&r.body), content, "{ctx}");
            let r = fetch(proto, ports, "GET", path, &[], b"");
            assert_eq!(r.header("content-encoding"), None, "{ctx}");
            assert_eq!(r.body, content, "{ctx}");
        }
    }
    // B-273: `buffering = "full"` の経路は全プロトコルで数える（以前は HTTP/1.1 だけ）
    let metrics = http1_get(ports.clear, "/__metrics").unwrap().text();
    let line = format!(
        "veil_proxy_buffering_used_total{{host=\"other\"}} {}",
        protos.len() * 2
    );
    assert!(metrics.contains(&line), "{line} missing\n{metrics}");
}

/// B-272・B-273: サーキットブレーカーが HTTP/2・HTTP/3 のクライアントの要求でも上流の失敗を数えて開き、
/// 状態のメトリクスも全プロトコルで更新される（以前は HTTP/1.1 の要求だけ）
#[test]
fn b272_circuit_breaker_opens_on_every_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    let ports = Ports::alloc();
    let protos = all_protos();
    let mut upstreams = String::new();
    let mut routes = String::new();
    let mut seen = Vec::new();
    // 失敗する上流と健全な上流の 2 台。全台が Open のときは健全なものへ戻す（全滅回避）ので 1 台では見えない
    let (good, _) = spawn_recording_upstream(|_| (200, String::new(), b"ok".to_vec()));
    for (i, _) in protos.iter().enumerate() {
        let (port, s) = spawn_recording_upstream(|_| (500, String::new(), b"boom".to_vec()));
        seen.push(s);
        upstreams.push_str(&format!(
            r#"
[upstreams."cb{i}"]
servers = ["http://127.0.0.1:{port}", "http://127.0.0.1:{good}"]
[upstreams."cb{i}".circuit_breaker]
enabled = true
failure_threshold = 2
failure_window_secs = 60
open_duration_secs = 120
"#
        ));
        routes.push_str(&format!(
            r#"
[[route]]
[route.conditions]
path = "/cb{i}"
[route.action]
type = "Proxy"
upstream = "cb{i}"
"#
        ));
    }
    let config = format!(
        "{}\n[prometheus]\nenabled = true\n{upstreams}{routes}",
        listeners_toml(ports, &cert, &key)
    );
    let veil = spawn_config(dir.path(), ports, &config);
    for (i, proto) in protos.iter().enumerate() {
        let path = format!("/cb{i}");
        let statuses: Vec<u16> = (0..10)
            .map(|_| fetch(*proto, ports, "GET", &path, &[], b"").status)
            .collect();
        // 2 回失敗したら Open になり、以降は健全な上流だけへ送る
        let hits = seen[i].lock().unwrap().len();
        assert_eq!(
            hits,
            2,
            "{proto:?}: failing upstream hit {hits} times {statuses:?}\n{}",
            veil.log()
        );
        assert!(
            statuses[6..].iter().all(|s| *s == 200),
            "{proto:?}: {statuses:?}"
        );
    }
    let metrics = http1_get(ports.clear, "/__metrics").unwrap().text();
    for (i, proto) in protos.iter().enumerate() {
        let line = format!("veil_circuit_breaker_state{{upstream=\"cb{i}\"}} 1");
        assert!(
            metrics.contains(&line),
            "{proto:?}: {line} missing\n{metrics}"
        );
        let line = format!("veil_circuit_breaker_open_total{{upstream=\"cb{i}\"}} 1");
        assert!(
            metrics.contains(&line),
            "{proto:?}: {line} missing\n{metrics}"
        );
    }
}

/// 本文を一部だけ送って止め、応答（ステータス）かストリームの打ち切りを待つ。
/// (結果, 待った時間)。Err はストリームが打ち切られた
fn stalled_upload(proto: Proto, ports: Ports, path: &str) -> (Result<u16, String>, Duration) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let path = path.to_string();
    rt.block_on(async move {
        let started = Instant::now();
        let run = async {
            match proto {
                Proto::H2c | Proto::H2Tls => {
                    let tls = proto == Proto::H2Tls;
                    let port = if tls { ports.tls } else { ports.clear };
                    let mut client = h2_connect(tls, port).await?;
                    let req = h2_request(tls, port, "POST", &path, &[]);
                    let (resp, mut send) =
                        client.send_request(req, false).map_err(|e| e.to_string())?;
                    send.send_data(bytes::Bytes::from(vec![b'x'; 100]), false)
                        .map_err(|e| e.to_string())?;
                    let resp = resp.await.map_err(|e| e.to_string())?;
                    // 送信側を持ったまま応答を受ける（drop すると RST_STREAM を送る）
                    drop(send);
                    Ok(resp.status().as_u16())
                }
                #[cfg(feature = "http3")]
                Proto::H3 => {
                    let (_endpoint, mut send) = h3_connect(ports.tls).await?;
                    let req = h3_request(ports.tls, "POST", &path, &[]);
                    let mut stream = send.send_request(req).await.map_err(|e| e.to_string())?;
                    stream
                        .send_data(bytes::Bytes::from(vec![b'x'; 100]))
                        .await
                        .map_err(|e| e.to_string())?;
                    let resp = stream.recv_response().await.map_err(|e| e.to_string())?;
                    Ok(resp.status().as_u16())
                }
                _ => unreachable!(),
            }
        };
        let r = tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .unwrap_or_else(|_| Err("client timed out (no reset from veil)".to_string()));
        (r, started.elapsed())
    })
}

/// B-274: ルートの `client_body_timeout_secs` が HTTP/2・HTTP/3 でも効き、本文が止まった要求を打ち切る
/// （ストリーミングの中継・バッファ経路とも。以前は HTTP/1.1 だけで、HTTP/2・HTTP/3 はいつまでも待った）
#[test]
fn b274_client_body_timeout_applies_to_h2_and_h3() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    let (up, seen) = spawn_recording_upstream(|_| (200, String::new(), b"ok".to_vec()));
    let ports = Ports::alloc();
    let timeout = r#"
[route.security]
client_body_timeout_secs = 2
"#;
    let config = format!(
        r#"{listeners}
[[route]]
[route.conditions]
path = "/stream"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up}/u"
{timeout}

[[route]]
[route.conditions]
path = "/buffered"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up}/u"
[route.buffering]
mode = "full"
{timeout}
"#,
        listeners = listeners_toml(ports, &cert, &key),
    );
    let veil = spawn_config(dir.path(), ports, &config);
    for proto in all_protos().into_iter().filter(|p| *p != Proto::H1) {
        for path in ["/stream", "/buffered"] {
            let (r, waited) = stalled_upload(proto, ports, path);
            let ctx = format!("{proto:?} {path}: {r:?} after {waited:?}\n{}", veil.log());
            // 打ち切り（リセット）か 408。どちらでも 2 秒の設定の前後で終わる
            assert!(
                matches!(r, Err(ref e) if !e.contains("client timed out")) || r == Ok(408),
                "{ctx}"
            );
            assert!(
                waited >= Duration::from_millis(1500) && waited < Duration::from_secs(8),
                "{ctx}"
            );
        }
    }
    assert!(
        seen.lock().unwrap().iter().all(|s| !s.complete),
        "a stalled upload reached the upstream as a complete request"
    );
}

/// 本文を一部送ってからストリームをリセットし、少し待つ
fn reset_mid_upload(proto: Proto, ports: Ports, path: &str) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let path = path.to_string();
    rt.block_on(async move {
        let chunk = bytes::Bytes::from(vec![b'y'; 20 * 1024]);
        match proto {
            Proto::H2c | Proto::H2Tls => {
                let tls = proto == Proto::H2Tls;
                let port = if tls { ports.tls } else { ports.clear };
                let mut client = h2_connect(tls, port).await.unwrap();
                let req = h2_request(tls, port, "POST", &path, &[]);
                let (_resp, mut send) = client.send_request(req, false).unwrap();
                send.reserve_capacity(chunk.len());
                send.send_data(chunk, false).unwrap();
                tokio::time::sleep(Duration::from_millis(700)).await;
                send.send_reset(h2::Reason::CANCEL);
                // 接続は閉じずに待つ（接続ごと閉じた場合と区別する）
                tokio::time::sleep(Duration::from_millis(1500)).await;
            }
            #[cfg(feature = "http3")]
            Proto::H3 => {
                let (_endpoint, mut send) = h3_connect(ports.tls).await.unwrap();
                let req = h3_request(ports.tls, "POST", &path, &[]);
                let mut stream = send.send_request(req).await.unwrap();
                stream.send_data(chunk).await.unwrap();
                tokio::time::sleep(Duration::from_millis(700)).await;
                stream.stop_stream(h3::error::Code::H3_REQUEST_CANCELLED);
                tokio::time::sleep(Duration::from_millis(1500)).await;
            }
            _ => unreachable!(),
        }
    });
}

/// B-277: アップロードの途中でクライアントがストリームをリセットしたら、上流（HTTP/1.1・h2c）へは終わった
/// 要求に見せずに打ち切る（以前は chunked の終端・END_STREAM を送り、途中までの本文が完全な要求として届いた）
#[test]
fn b277_client_reset_mid_upload_is_not_completed_upstream() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    let (up1, seen1) = spawn_recording_upstream(|_| (200, String::new(), b"ok".to_vec()));
    let (up2, seen2) = spawn_recording_h2c_upstream();
    let ports = Ports::alloc();
    let config = format!(
        r#"{listeners}
[[route]]
[route.conditions]
path = "/h1"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up1}/h1"

[[route]]
[route.conditions]
path = "/h2"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up2}/h2"
use_h2c = true
"#,
        listeners = listeners_toml(ports, &cert, &key),
    );
    let veil = spawn_config(dir.path(), ports, &config);
    for proto in all_protos().into_iter().filter(|p| *p != Proto::H1) {
        let before1 = seen1.lock().unwrap().len();
        let before2 = seen2.lock().unwrap().len();
        reset_mid_upload(proto, ports, "/h1");
        reset_mid_upload(proto, ports, "/h2");
        let ok = wait_until(Duration::from_secs(10), || {
            seen1.lock().unwrap().len() > before1 && seen2.lock().unwrap().len() > before2
        });
        let s1 = seen1.lock().unwrap()[before1..].to_vec();
        let s2 = seen2.lock().unwrap()[before2..].to_vec();
        let ctx = format!("{proto:?}: h1 {s1:?} h2 {s2:?}\n{}", veil.log());
        assert!(ok, "upstream did not see the aborted request end: {ctx}");
        assert!(
            s1.iter()
                .all(|s| !s.complete && s.head.starts_with("POST /h1") && s.body.is_empty()),
            "{ctx}"
        );
        assert!(
            s2.iter()
                .all(|s| !s.complete && s.path.starts_with("/h2") && s.body_len <= 20 * 1024),
            "{ctx}"
        );
    }
}

/// 応答本文を `REWRITTEN-BODY` に置き換える Proxy-Wasm モジュール（WAT）
#[cfg(feature = "wasm")]
const RESPONSE_BODY_REWRITER: &str = r#"
(module
  (import "env" "proxy_set_buffer_bytes"
    (func $set_buffer (param i32 i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 2)
  (data (i32.const 16) "REWRITTEN-BODY")
  (global $bump (mut i32) (i32.const 65536))
  (func (export "proxy_on_memory_allocate") (param $size i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get $size)))
    (local.get $p))
  (func (export "proxy_on_response_body") (param $ctx i32) (param $len i32) (param $eos i32) (result i32)
    (if (local.get $eos)
      (then
        (drop (call $set_buffer (i32.const 1) (i32.const 0) (local.get $len) (i32.const 16) (i32.const 14)))))
    (i32.const 0))
)
"#;

/// 応答ヘッダに `x-wasm-resp: 1` を足す Proxy-Wasm モジュール（WAT）
#[cfg(feature = "wasm")]
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

/// B-268: Proxy-Wasm の `on_http_response_headers` が上流（HTTP/1.1・h2c）・圧縮・`buffering = "full"` の
/// どの経路でも全プロトコルで呼ばれる（以前は HTTP/2 → HTTP/1.1 の上流・HTTP/1.1 → h2c の上流・HTTP/1.1 の
/// 圧縮とバッファの経路で呼ばれなかった）
#[cfg(feature = "wasm")]
#[test]
fn b268_wasm_response_headers_on_every_path() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    let wat = dir.path().join("adder.wat");
    std::fs::write(&wat, RESPONSE_HEADER_ADDER).unwrap();
    let (up, _seen) = spawn_recording_upstream(|_| {
        (
            200,
            "Content-Type: text/plain\r\n".to_string(),
            vec![b'u'; 2000],
        )
    });
    let www = dir.path().join("www");
    std::fs::create_dir_all(&www).unwrap();
    std::fs::write(www.join("o.txt"), vec![b'o'; 2000]).unwrap();
    let ports = Ports::alloc();
    let route = |path: &str, url: &str, extra: &str| {
        format!(
            r#"
[[route]]
modules = ["adder"]
[route.conditions]
path = "{path}"
[route.action]
type = "Proxy"
url = "{url}"
{extra}
"#
        )
    };
    let h1 = format!("http://127.0.0.1:{up}/x");
    let h2 = format!("http://127.0.0.1:{}/origin", ports.clear);
    let gz =
        "[route.compression]\nenabled = true\npreferred_encodings = [\"gzip\"]\nmin_size = 100\n";
    let buf = "[route.buffering]\nmode = \"full\"\n";
    let config = format!(
        r#"{listeners}
[wasm]
enabled = true

[[wasm.modules]]
name = "adder"
path = "{wat}"
[wasm.modules.capabilities]
allow_response_headers_read = true
allow_response_headers_write = true

[[route]]
[route.conditions]
path = "/origin"
[route.action]
type = "File"
path = "{www}/o.txt"
mode = "memory"
{r1}{r2}{r3}{r4}{r5}"#,
        listeners = listeners_toml(ports, &cert, &key),
        wat = tp(&wat),
        www = tp(&www),
        r1 = route("/h1", &h1, ""),
        r2 = route("/h2", &h2, "use_h2c = true"),
        r3 = route("/h1-gz", &h1, gz),
        r4 = route("/h1-buf", &h1, buf),
        r5 = route("/h2-gz", &h2, &format!("use_h2c = true\n{gz}")),
    );
    let veil = spawn_config(dir.path(), ports, &config);
    for proto in all_protos() {
        for path in ["/h1", "/h2", "/h1-gz", "/h1-buf", "/h2-gz"] {
            let r = fetch(
                proto,
                ports,
                "GET",
                path,
                &[("accept-encoding", "gzip")],
                b"",
            );
            let ctx = format!("{proto:?} {path} {:?}\n{}", r.headers, veil.log());
            assert_eq!(r.status, 200, "{ctx}");
            assert_eq!(r.header("x-wasm-resp"), Some("1"), "{ctx}");
            let body = if r.header("content-encoding") == Some("gzip") {
                gunzip(&r.body)
            } else {
                r.body.clone()
            };
            assert_eq!(body.len(), 2000, "{ctx}");
            if path.ends_with("-gz") {
                assert_eq!(r.header("content-encoding"), Some("gzip"), "{ctx}");
            }
        }
    }
}

/// F-285: Proxy-Wasm の `on_http_response_body` が HTTP/1.1・HTTP/2 でも呼ばれ、書き換えた本文と
/// Content-Length・圧縮が全プロトコルでそろう（上流は HTTP/1.1・h2c）
#[cfg(feature = "wasm")]
#[test]
fn f285_wasm_response_body_rewrite_on_every_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    let wat = dir.path().join("rewriter.wat");
    std::fs::write(&wat, RESPONSE_BODY_REWRITER).unwrap();
    let (up, _seen) = spawn_recording_upstream(|_| {
        (
            200,
            "Content-Type: text/plain\r\n".to_string(),
            b"original upstream body".to_vec(),
        )
    });
    let www = dir.path().join("www");
    std::fs::create_dir_all(&www).unwrap();
    std::fs::write(www.join("o.txt"), "original h2c origin body").unwrap();
    let ports = Ports::alloc();
    let config = format!(
        r#"{listeners}
[wasm]
enabled = true

[[wasm.modules]]
name = "rewriter"
path = "{wat}"
[wasm.modules.capabilities]
allow_response_body_read = true
allow_response_body_write = true

[[route]]
[route.conditions]
path = "/origin"
[route.action]
type = "File"
path = "{www}/o.txt"
mode = "memory"

[[route]]
modules = ["rewriter"]
[route.conditions]
path = "/h1up"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up}/x"

[[route]]
modules = ["rewriter"]
[route.conditions]
path = "/h2up"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{clear}/origin"
use_h2c = true

[[route]]
modules = ["rewriter"]
[route.conditions]
path = "/h1up-gz"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up}/x"
[route.compression]
enabled = true
preferred_encodings = ["gzip"]
min_size = 1
"#,
        listeners = listeners_toml(ports, &cert, &key),
        wat = tp(&wat),
        www = tp(&www),
        clear = ports.clear,
    );
    let veil = spawn_config(dir.path(), ports, &config);
    for proto in all_protos() {
        for path in ["/h1up", "/h2up"] {
            let ctx = format!("{proto:?} {path}\n{}", veil.log());
            let r = fetch(proto, ports, "GET", path, &[], b"");
            assert_eq!(r.status, 200, "{ctx}");
            assert_eq!(r.text(), "REWRITTEN-BODY", "{ctx}");
            if let Some(cl) = r.header("content-length") {
                assert_eq!(cl, "14", "{ctx}");
            }
        }
        let r = fetch(
            proto,
            ports,
            "GET",
            "/h1up-gz",
            &[("accept-encoding", "gzip")],
            b"",
        );
        let ctx = format!("{proto:?} /h1up-gz {:?}\n{}", r.headers, veil.log());
        assert_eq!(r.status, 200, "{ctx}");
        assert_eq!(r.header("content-encoding"), Some("gzip"), "{ctx}");
        assert_eq!(gunzip(&r.body), b"REWRITTEN-BODY", "{ctx}");
    }
}

/// 応答だけを決まった形で返す上流（HTTP/1.1）。要求の行のパスで振る舞いを選ぶ。
/// `/sse-chunked`・`/sse-close` は最初のイベントを送って閉じずに待つ。`/trunc-cl`・`/trunc-chunked` は本文の途中で閉じる
fn spawn_scripted_upstream() -> u16 {
    use std::io::{Read, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut tmp) {
                        Ok(n) if n > 0 => buf.extend_from_slice(&tmp[..n]),
                        _ => return,
                    }
                }
                let head = String::from_utf8_lossy(&buf).into_owned();
                let path = head.split(' ').nth(1).unwrap_or("").to_string();
                let event = b"data: hello\n\n";
                if path.starts_with("/sse-chunked") {
                    let _ = s.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
                    );
                    let _ = s.write_all(format!("{:x}\r\n", event.len()).as_bytes());
                    let _ = s.write_all(event);
                    let _ = s.write_all(b"\r\n");
                    std::thread::sleep(Duration::from_secs(20));
                } else if path.starts_with("/sse-close") {
                    // 長さも chunked も無い（閉じるまでが本文）
                    let _ = s.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                    );
                    let _ = s.write_all(event);
                    std::thread::sleep(Duration::from_secs(20));
                } else if path.starts_with("/trunc-cl") {
                    let _ = s.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: 100000\r\n\r\n",
                    );
                    let _ = s.write_all(&[b'x'; 1000]);
                } else if path.starts_with("/trunc-chunked") {
                    let _ = s.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nTransfer-Encoding: chunked\r\n\r\n3e8\r\n",
                    );
                    let _ = s.write_all(&[b'x'; 1000]);
                    let _ = s.write_all(b"\r\n");
                }
                // 本文の途中で閉じる（`/trunc-*`）
            });
        }
    });
    port
}

/// B-281: 圧縮の効くルートの SSE（`text/event-stream`）が、最後まで溜められずにすぐ届く。
/// 以前は圧縮のために本文を最後まで溜めていたので、終わらない SSE は何も届かなかった
/// （HTTP/2 は圧縮が無くても、長さ不明の応答を溜めていた）
#[test]
fn b281_sse_streams_through_compression_route_on_every_protocol() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    let up = spawn_scripted_upstream();
    let ports = Ports::alloc();
    let config = format!(
        r#"{listeners}
[[route]]
[route.conditions]
path = "/c"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up}"
[route.compression]
enabled = true
preferred_encodings = ["gzip"]
min_size = 1

[[route]]
[route.conditions]
path = "/p"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up}"
"#,
        listeners = listeners_toml(ports, &cert, &key),
    );
    let veil = spawn_config(dir.path(), ports, &config);
    for proto in all_protos() {
        for path in [
            "/c/sse-chunked",
            "/c/sse-close",
            "/p/sse-chunked",
            "/p/sse-close",
        ] {
            let r = fetch_until(
                proto,
                ports,
                path,
                &[("accept-encoding", "gzip")],
                b"data: hello",
                Duration::from_secs(5),
            )
            .unwrap_or_else(|e| panic!("{proto:?} {path}: {e}\n{}", veil.log()));
            assert_eq!(r.status, 200, "{proto:?} {path}");
            assert_eq!(r.header("content-encoding"), None, "{proto:?} {path}");
        }
    }
}

/// B-282: 上流が本文の途中で閉じた応答を、HTTP/2・HTTP/3 のクライアントへ完結した応答に見せない
/// （ストリームのリセットで打ち切る）。以前の HTTP/2 は空の END_STREAM で閉じていた
#[test]
fn b282_truncated_upstream_body_resets_h2_and_h3_streams() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    let up = spawn_scripted_upstream();
    let ports = Ports::alloc();
    let config = format!(
        r#"{listeners}
[[route]]
[route.conditions]
path = "/p"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{up}"
"#,
        listeners = listeners_toml(ports, &cert, &key),
    );
    let veil = spawn_config(dir.path(), ports, &config);
    for proto in all_protos() {
        if proto == Proto::H1 {
            continue;
        }
        for path in ["/p/trunc-cl", "/p/trunc-chunked"] {
            match try_fetch(proto, ports, path) {
                Err(_) => {}
                Ok(r) if r.status == 502 => {}
                Ok(r) => panic!(
                    "{proto:?} {path}: truncated body looked complete (status {}, {} bytes)\n{}",
                    r.status,
                    r.body.len(),
                    veil.log()
                ),
            }
        }
    }
}

/// B-285: END_STREAM 付き・END_HEADERS 無しの HEADERS の続き（CONTINUATION）を送らずに止めたクライアントも、
/// `[http2] stream_idle_timeout_secs` で打ち切る（以前はストリームの状態が HalfClosedRemote のため見回りから漏れ、
/// 接続のアイドルタイムアウトも効かずにいつまでも保持した）
#[test]
fn b285_stalled_continuation_is_reset_on_h2() {
    use std::io::{Read, Write};
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    let ports = Ports::alloc();
    let config = format!(
        r#"{listeners}
[http2]
stream_idle_timeout_secs = 2

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "Respond"
status = 200
body = "ok"
"#,
        listeners = listeners_toml(ports, &cert, &key),
    );
    let veil = spawn_config(dir.path(), ports, &config);
    let mut s = std::net::TcpStream::connect(("127.0.0.1", ports.clear)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n").unwrap();
    // SETTINGS（空）
    s.write_all(&[0, 0, 0, 0x4, 0, 0, 0, 0, 0]).unwrap();
    // HEADERS（stream 1、END_STREAM のみ。END_HEADERS 無し）: `:method GET` だけで止める
    s.write_all(&[0, 0, 1, 0x1, 0x1, 0, 0, 0, 1, 0x82]).unwrap();
    let started = Instant::now();
    // RST_STREAM（type 3）か GOAWAY（type 7）か接続の終わりが来るまで読む
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let outcome = loop {
        let mut off = 0;
        let mut found = None;
        while buf.len() >= off + 9 {
            let len = ((buf[off] as usize) << 16)
                | ((buf[off + 1] as usize) << 8)
                | buf[off + 2] as usize;
            if matches!(buf[off + 3], 3 | 7) {
                found = Some(format!("frame type {}", buf[off + 3]));
                break;
            }
            off += 9 + len;
        }
        if let Some(f) = found {
            break f;
        }
        match s.read(&mut chunk) {
            Ok(0) => break "closed".to_string(),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) => break format!("read error: {e}"),
        }
    };
    let waited = started.elapsed();
    assert!(
        !outcome.starts_with("read error") && waited >= Duration::from_millis(1500),
        "{outcome} after {waited:?}\n{}",
        veil.log()
    );
}
