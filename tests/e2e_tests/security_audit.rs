//! F-236 の監査（再走査）で直した要求の扱い: パスの正規化（B-148）・chunked の要求の密輸（B-142・B-143）
//!
//! `/api/ip-restricted/*` は 127.0.0.0/8 を拒否するルート（`/*` は同じ上流へ通す）。正規化しない・終端の後ろを
//! 送る実装では、ここへの要求が `/*` として上流へ届く。

use super::ktls_h2_ws_edge::send_raw_tls_request;
use super::*;

/// 応答に含まれるステータス行のコードを順に返す
fn status_codes(resp: &str) -> Vec<u16> {
    resp.match_indices("HTTP/1.1 ")
        .filter_map(|(i, _)| resp.get(i + 9..i + 12)?.parse().ok())
        .collect()
}

/// B-148: 符号化・ドットのセグメント・絶対形式でルートの制限を素通りできない
#[tokio::test]
#[ntest::timeout(30000)]
pub(super) async fn test_f236_path_normalization_keeps_route_restrictions() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    for target in [
        "/api/ip-restricted/x",
        "/api/%69p-restricted/x",
        "/api/./ip-restricted/x",
        "/api/zzz/../ip-restricted/x",
        "https://localhost/api/ip-restricted/x",
    ] {
        let req = format!(
            "GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            target
        );
        let resp = send_raw_tls_request(req.as_bytes()).expect("response");
        assert_eq!(
            status_codes(&resp).first().copied(),
            Some(403),
            "{} は拒否のルートに一致すること: {}",
            target,
            resp.lines().next().unwrap_or("")
        );
    }
    // origin 形式でない要求先は 400
    let resp =
        send_raw_tls_request(b"GET api/x HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .expect("response");
    assert_eq!(status_codes(&resp).first().copied(), Some(400), "{}", resp);
}

/// B-142: chunked の本文の終端の後ろの要求は、veil が照合・検査する次の要求として扱う
#[tokio::test]
#[ntest::timeout(30000)]
pub(super) async fn test_f236_chunked_trailing_request_is_routed_by_veil() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    let req = b"POST / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\nGET /api/ip-restricted/x HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    let resp = send_raw_tls_request(req).expect("response");
    let codes = status_codes(&resp);
    assert!(
        codes.contains(&403),
        "後ろの要求は拒否のルートで 403 になること（上流へ素通りしない）: {:?}",
        codes
    );
}

/// B-143: chunked の不正なフレーミングは 400
#[tokio::test]
#[ntest::timeout(30000)]
pub(super) async fn test_f236_malformed_chunked_request_rejected() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    for body in [
        &b"4x\r\nWiki\r\n0\r\n\r\n"[..],
        &b"4\nWiki\r\n0\r\n\r\n"[..],
        &b"4\r\nWikiXX0\r\n\r\n"[..],
    ] {
        let mut req = b"POST / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
        req.extend_from_slice(body);
        let resp = send_raw_tls_request(&req).expect("response");
        let first = status_codes(&resp).first().copied();
        assert!(
            first == Some(400) || first.is_none(),
            "不正な chunked は 400（または接続を閉じる）: {:?} {:?}",
            String::from_utf8_lossy(body),
            first
        );
        assert_ne!(first, Some(200), "上流へ渡して成功にしないこと");
    }
}

/// B-219: `VEIL_E2E_SECCOMP` で seccomp を掛けたとき、制御プレーンを含む全スレッドに掛かっている
/// （prctl は呼んだスレッドにしか掛からず、先に作ったスレッドが無制限のまま動いていた）
#[cfg(target_os = "linux")]
#[tokio::test]
#[ntest::timeout(20000)]
pub(super) async fn test_b219_seccomp_on_every_thread() {
    if std::env::var("VEIL_E2E_SECCOMP").map_or(true, |m| m.is_empty()) {
        eprintln!("Skipping test: VEIL_E2E_SECCOMP is not set");
        return;
    }
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    let out = std::process::Command::new("pgrep")
        .args(["-f", "veil.*proxy.toml"])
        .output()
        .expect("pgrep");
    let pids = String::from_utf8_lossy(&out.stdout);
    let pid = pids
        .lines()
        .next()
        .and_then(|l| l.trim().parse::<u32>().ok())
        .expect("proxy pid");
    let mut threads = 0;
    for entry in std::fs::read_dir(format!("/proc/{pid}/task")).expect("task dir") {
        let task = entry.expect("task").path();
        let status = std::fs::read_to_string(task.join("status")).unwrap_or_default();
        let name = std::fs::read_to_string(task.join("comm")).unwrap_or_default();
        let mode = status
            .lines()
            .find_map(|l| l.strip_prefix("Seccomp:"))
            .map(str::trim)
            .unwrap_or("?");
        assert_eq!(
            mode,
            "2",
            "thread {} ({}) is not under the seccomp filter",
            task.display(),
            name.trim()
        );
        threads += 1;
    }
    assert!(threads > 1, "expected several threads, found {threads}");
    eprintln!("seccomp filter on all {threads} threads of pid {pid}");
}
