//! 自己完結型 E2E（veil バイナリをテストごとに一時設定で起動する）。
//!
//! `tests/e2e_setup.sh` が立てる共有環境（`e2e_tests.rs`）とは別に、プロセスの起動条件
//! （環境変数・設定ファイルの差し替え・シグナル）そのものを検証したいテストをここに置く。
//! バイナリは cargo が `CARGO_BIN_EXE_veil` で渡す（`cargo test --test standalone_e2e --features full`）。
//!
//! - F-179: 環境変数 `VEIL_OVERRIDE_<n>` による設定上書き（SIGHUP 後も保たれる）
//! - F-180: 証明書の Kubernetes Secret 形式（`..data` シンボリックリンク差し替え）への追従
//! - F-188 / F-200: TLS 無しの平文専用サーバ（`[server].listen_cleartext`）、`allow_http` / `allow_h2c` /
//!   `enable_secure_redirect`、削除されたキーのエラー、HTTP/3 の検証規則
// 理由付き allow: テストハーネスは同期 I/O・sleep・std::net を意図的に使う（データプレーンとは別プロセス）。
#![allow(clippy::disallowed_methods)]

mod standalone;

use standalone::*;
use std::time::Duration;

// ===================== F-179 =====================

/// 環境変数で listen を上書きして起動し、SIGHUP で設定ファイルを読み直しても上書きが保たれる。
#[test]
fn f179_env_override_listen_and_survives_sighup() {
    let dir = tempfile::tempdir().unwrap();
    let www = dir.path().join("www");
    std::fs::create_dir_all(&www).unwrap();
    std::fs::write(www.join("index.html"), "v1").unwrap();

    let file_port = free_port();
    let env_port = free_port();
    let config = format!(
        r#"
[server]
listen_cleartext = "127.0.0.1:{file_port}"
enable_secure_redirect = false
threads = 1
[logging]
level = "warn"
[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "{www}"
"#,
        www = tp(&www),
    );
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, &config).unwrap();

    let veil = VeilProcess::spawn(
        &cfg_path,
        &[(
            "VEIL_OVERRIDE_1",
            format!(r#"server.listen_cleartext = "127.0.0.1:{env_port}""#).as_str(),
        )],
        &[],
    );
    assert!(
        wait_for_port(env_port, Duration::from_secs(20)),
        "veil did not listen on the env-overridden port; log:\n{}",
        veil.log()
    );
    let resp = http1_get(env_port, "/index.html").expect("request on overridden port");
    assert_eq!(resp.status, 200, "{:?}", resp);
    assert_eq!(resp.body, b"v1");
    assert!(
        http1_get(file_port, "/index.html").is_err(),
        "the TOML port must not be bound when overridden"
    );

    // 設定ファイルのルートを変えて SIGHUP。listen の上書きは保たれ、ルートの変更は反映される。
    // Windows に SIGHUP は無い。
    #[cfg(unix)]
    reload_keeps_env_override(&veil, &dir, &cfg_path, &config, &www, env_port);
}

#[cfg(unix)]
fn reload_keeps_env_override(
    veil: &VeilProcess,
    dir: &tempfile::TempDir,
    cfg_path: &std::path::Path,
    config: &str,
    www: &std::path::Path,
    env_port: u16,
) {
    let www2 = dir.path().join("www2");
    std::fs::create_dir_all(&www2).unwrap();
    std::fs::write(www2.join("index.html"), "v2").unwrap();
    std::fs::write(cfg_path, config.replace(&tp(www), &tp(&www2))).unwrap();
    veil.sighup();
    let ok = wait_until(Duration::from_secs(10), || {
        http1_get(env_port, "/index.html")
            .map(|r| r.body == b"v2")
            .unwrap_or(false)
    });
    assert!(ok, "reload not applied; log:\n{}", veil.log());
}

/// 環境変数で `route.0.action.path` のような配列添字つきのパスも上書きできる。
/// CLI の `--override` は環境変数より優先される。
#[test]
fn f179_env_override_array_index_and_cli_priority() {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in [("a", "A"), ("b", "B"), ("c", "C")] {
        let d = dir.path().join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("index.html"), body).unwrap();
    }
    let port = free_port();
    let config = format!(
        r#"
[server]
listen_cleartext = "127.0.0.1:{port}"
enable_secure_redirect = false
threads = 1
[logging]
level = "warn"
[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "{a}"
"#,
        a = tp(&dir.path().join("a")),
    );
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, &config).unwrap();

    let b = tp(&dir.path().join("b"));
    let c = tp(&dir.path().join("c"));
    let env_value = format!(r#"route.0.action.path = "{b}""#);
    let cli_value = format!(r#"route.0.action.path = "{c}""#);

    // 環境変数だけ → B
    {
        let veil = VeilProcess::spawn(&cfg_path, &[("VEIL_OVERRIDE_7", env_value.as_str())], &[]);
        assert!(
            wait_for_port(port, Duration::from_secs(20)),
            "{}",
            veil.log()
        );
        let resp = http1_get(port, "/index.html").unwrap();
        assert_eq!(resp.body, b"B", "{}", veil.log());
    }
    assert!(wait_until(Duration::from_secs(10), || http1_get(port, "/").is_err()));
    // 環境変数 + CLI → CLI（C）が勝つ
    {
        let veil = VeilProcess::spawn(
            &cfg_path,
            &[("VEIL_OVERRIDE_7", env_value.as_str())],
            &["--override", cli_value.as_str()],
        );
        assert!(
            wait_for_port(port, Duration::from_secs(20)),
            "{}",
            veil.log()
        );
        let resp = http1_get(port, "/index.html").unwrap();
        assert_eq!(resp.body, b"C", "{}", veil.log());
    }
}

/// 不正な名前の環境変数は起動エラーになる（黙って無視しない）。
#[test]
fn f179_env_override_bad_name_fails_startup() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, "[server]\nlisten = \"127.0.0.1:1\"\n").unwrap();
    let out = std::process::Command::new(veil_bin())
        .arg("-c")
        .arg(&cfg_path)
        .arg("-t")
        .env("VEIL_OVERRIDE_X", "server.threads = 1")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("VEIL_OVERRIDE_X"), "{}", stderr);
}

// ===================== F-180 =====================

/// Kubernetes Secret のボリューム形式（`tls.crt -> ..data/tls.crt`、`..data -> ..<timestamp>`）で
/// 証明書を渡し、`..data` を付け替えると `auto_reload` で新しい証明書に替わる。
#[cfg(unix)]
#[test]
fn f180_k8s_secret_symlink_swap_reloads_certificate() {
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("secret");
    let v1 = secret.join("..2026_10_12_01");
    let v2 = secret.join("..2026_10_12_02");
    std::fs::create_dir_all(&v1).unwrap();
    std::fs::create_dir_all(&v2).unwrap();
    write_self_signed_named(&v1, "tls.crt", "tls.key", "v1.example");
    write_self_signed_named(&v2, "tls.crt", "tls.key", "v2.example");
    let data = secret.join("..data");
    std::os::unix::fs::symlink("..2026_10_12_01", &data).unwrap();
    std::os::unix::fs::symlink("..data/tls.crt", secret.join("tls.crt")).unwrap();
    std::os::unix::fs::symlink("..data/tls.key", secret.join("tls.key")).unwrap();

    let www = dir.path().join("www");
    std::fs::create_dir_all(&www).unwrap();
    std::fs::write(www.join("index.html"), "ok").unwrap();
    let port = free_port();
    let config = format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
threads = 1
[tls]
cert_path = "{secret}/tls.crt"
key_path = "{secret}/tls.key"
auto_reload = true
reload_interval_secs = 1
[logging]
level = "info"
[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "{www}"
"#,
        secret = tp(&secret),
        www = tp(&www),
    );
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, config).unwrap();
    let mut veil = VeilProcess::spawn(&cfg_path, &[], &[]);
    assert!(
        wait_for_port(port, Duration::from_secs(20)),
        "{}",
        veil.log()
    );

    let first = tls_peer_cert(port, "localhost").expect("handshake");
    let v1_der = pem_to_der(&std::fs::read_to_string(v1.join("tls.crt")).unwrap());
    let v2_der = pem_to_der(&std::fs::read_to_string(v2.join("tls.crt")).unwrap());
    assert_eq!(first, v1_der);

    // kubelet と同じ手順（一時リンク → rename）で付け替える。
    let tmp = secret.join("..data_tmp");
    std::os::unix::fs::symlink("..2026_10_12_02", &tmp).unwrap();
    std::fs::rename(&tmp, &data).unwrap();

    let ok = wait_until(Duration::from_secs(15), || {
        tls_peer_cert(port, "localhost")
            .map(|c| c == v2_der)
            .unwrap_or(false)
    });
    if !ok {
        let last = tls_peer_cert(port, "localhost").map(|c| c == v1_der);
        veil.terminate(Duration::from_secs(10));
        panic!(
            "certificate was not reloaded (still v1: {:?}); log:\n{}",
            last,
            veil.log()
        );
    }
}

#[cfg(unix)]
fn pem_to_der(pem: &str) -> Vec<u8> {
    use rustls::pki_types::pem::PemObject;
    rustls::pki_types::CertificateDer::from_pem_slice(pem.as_bytes())
        .unwrap()
        .as_ref()
        .to_vec()
}

// ===================== F-188（本体側の小改修） =====================

/// 平文専用サーバの設定（`listen_cleartext` のみ・`[tls]` 無し）を書き出して起動する。
fn spawn_cleartext(dir: &std::path::Path, port: u16, server_extra: &str) -> VeilProcess {
    let www = dir.join("www");
    std::fs::create_dir_all(&www).unwrap();
    std::fs::write(www.join("index.html"), "plain").unwrap();
    let config = format!(
        r#"
[server]
listen_cleartext = "127.0.0.1:{port}"
threads = 1
{server_extra}
[logging]
level = "warn"
[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "{www}"
"#,
        www = tp(&www)
    );
    let cfg_path = dir.join("config.toml");
    std::fs::write(&cfg_path, config).unwrap();
    let veil = VeilProcess::spawn(&cfg_path, &[], &[]);
    assert!(
        wait_for_port(port, Duration::from_secs(20)),
        "{}",
        veil.log()
    );
    veil
}

/// `[tls]` を省略した平文専用サーバ（`listen_cleartext` のみ）で、同じポートに
/// 平文 HTTP/1.1 と h2c（prior knowledge）の両方が来ても処理できる。
#[test]
fn f188_plaintext_only_server_without_tls_serves_http1_and_h2c() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let veil = spawn_cleartext(dir.path(), port, "enable_secure_redirect = false");

    let resp = http1_get(port, "/index.html").expect("plain HTTP/1.1");
    assert_eq!(resp.status, 200, "{}", veil.log());
    assert_eq!(resp.text(), "plain");
    assert!(resp
        .header("content-type")
        .unwrap_or("")
        .starts_with("text/html"));

    let (status, body) = h2c_get(port, "/index.html");
    assert_eq!(status, 200);
    assert_eq!(body, b"plain");
}

/// B-122: 平文リスナーは接続してから最初のバイトが届くまで待つ（以前は 200ms で打ち切り、HTTP/1.1 の
/// クライアントへ HTTP/2 の GOAWAY を返していた）。最初の断片が判別に足りなくても続きを待ち、
/// 何も送らずに閉じた接続（TCP のヘルスチェック）は WARN を出さずに閉じる。
#[test]
fn b122_cleartext_waits_for_late_and_fragmented_first_bytes() {
    use std::io::{Read, Write};
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let veil = spawn_cleartext(dir.path(), port, "enable_secure_redirect = false");

    let get = |chunks: &[(&[u8], u64)]| -> String {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        for (bytes, delay_ms) in chunks {
            std::thread::sleep(Duration::from_millis(*delay_ms));
            s.write_all(bytes).unwrap();
        }
        let mut buf = [0u8; 512];
        let n = s.read(&mut buf).unwrap_or(0);
        String::from_utf8_lossy(&buf[..n]).into_owned()
    };
    let req: &[u8] = b"GET /index.html HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";

    // 接続後 1 秒たってから要求を送る（ブラウザの事前接続・接続プール）
    let resp = get(&[(req, 1000)]);
    assert!(
        resp.starts_with("HTTP/1.1 200"),
        "late request: {resp:?}\n{}",
        veil.log()
    );
    // 判別に足りない断片（3 バイト）のあとに続き
    let resp = get(&[(&req[..3], 0), (&req[3..], 50)]);
    assert!(
        resp.starts_with("HTTP/1.1 200"),
        "fragmented request: {resp:?}\n{}",
        veil.log()
    );

    // 何も送らずに閉じる接続（TCP のヘルスチェック）を繰り返しても、その後の要求は通り WARN も出ない
    for _ in 0..5 {
        drop(std::net::TcpStream::connect(("127.0.0.1", port)).unwrap());
    }
    let resp = http1_get(port, "/index.html").expect("plain HTTP/1.1 after empty connections");
    assert_eq!(resp.status, 200, "{}", veil.log());
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !veil.log().contains("Unknown protocol"),
        "empty connections must not be logged as unknown protocol:\n{}",
        veil.log()
    );
}

/// B-123: HTTP/2 の要求本文を読み捨てる経路（上流に繋がらず 502 を返す等）でも、消費のたびに
/// メインループを起こして recv ウィンドウを補充する。以前は補充されず、接続の初期ウィンドウ（1MiB）を
/// 超える本文を送るクライアントが止まったまま応答が返らなかった（reactor で必ず固まる）。
#[test]
fn b123_h2c_large_body_to_failing_upstream_gets_502() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    // 誰も listen していないポート（接続は拒否される）
    let dead = free_port();
    let config = format!(
        r#"
[server]
listen_cleartext = "127.0.0.1:{port}"
enable_secure_redirect = false
threads = 1
[logging]
level = "warn"
[[route]]
[route.conditions]
path = "/up"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{dead}"
"#
    );
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, config).unwrap();
    let veil = VeilProcess::spawn(&cfg_path, &[], &[]);
    assert!(
        wait_for_port(port, Duration::from_secs(20)),
        "{}",
        veil.log()
    );
    for _ in 0..2 {
        let status = h2c_post(port, "/up", 3 * 1024 * 1024, Duration::from_secs(20));
        assert_eq!(status, Some(502), "{}", veil.log());
    }
}

/// F-200: `allow_http = false` は平文 HTTP/1.1 を 426 で拒否し、h2c は受ける。
#[test]
fn f200_allow_http_false_rejects_http1_with_426_but_serves_h2c() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let veil = spawn_cleartext(
        dir.path(),
        port,
        "enable_secure_redirect = false\nallow_http = false",
    );
    let resp = http1_get(port, "/index.html").expect("HTTP/1.1 is answered with 426");
    assert_eq!(resp.status, 426, "{}", veil.log());
    let (status, body) = h2c_get(port, "/index.html");
    assert_eq!(status, 200);
    assert_eq!(body, b"plain");
}

/// F-200: `allow_h2c = false` は h2c を拒否し、平文 HTTP/1.1 は受ける。
#[test]
fn f200_allow_h2c_false_rejects_h2c_but_serves_http1() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let veil = spawn_cleartext(
        dir.path(),
        port,
        "enable_secure_redirect = false\nallow_h2c = false",
    );
    let resp = http1_get(port, "/index.html").expect("plain HTTP/1.1");
    assert_eq!(resp.status, 200, "{}", veil.log());
    assert_eq!(resp.text(), "plain");
    let rejected = std::panic::catch_unwind(|| h2c_get(port, "/index.html"));
    assert!(
        rejected.is_err(),
        "h2c must be rejected when allow_h2c = false"
    );
}

/// F-200: `enable_secure_redirect = true`（要 `listen`）なら平文の HTTP/1.1・h2c とも
/// `https://<Host>:<listen のポート>` へ 301 を返す。
#[test]
fn f200_secure_redirect_answers_301_for_http1_and_h2c() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    let www = dir.path().join("www");
    std::fs::create_dir_all(&www).unwrap();
    std::fs::write(www.join("index.html"), "plain").unwrap();
    let tls_port = free_port();
    let port = free_port();
    let config = format!(
        r#"
[server]
listen = "127.0.0.1:{tls_port}"
listen_cleartext = "127.0.0.1:{port}"
threads = 1
[tls]
cert_path = "{cert}"
key_path = "{key}"
ktls_enabled = false
[logging]
level = "warn"
[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "{www}"
"#,
        cert = tp(&cert),
        key = tp(&key),
        www = tp(&www)
    );
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, config).unwrap();
    let veil = VeilProcess::spawn(&cfg_path, &[], &[]);
    assert!(
        wait_for_port(port, Duration::from_secs(20)),
        "{}",
        veil.log()
    );
    assert!(
        wait_for_port(tls_port, Duration::from_secs(20)),
        "{}",
        veil.log()
    );

    let want = format!("https://localhost:{tls_port}/index.html");
    let resp = http1_get(port, "/index.html").expect("HTTP/1.1");
    assert_eq!(resp.status, 301, "{}", veil.log());
    assert_eq!(resp.header("location"), Some(want.as_str()));

    let (status, headers, _) = h2c_get_with_headers(port, "/index.html");
    assert_eq!(status, 301, "{}", veil.log());
    let location = headers
        .iter()
        .find(|(n, _)| n == "location")
        .map(|(_, v)| v.as_str());
    assert_eq!(location, Some(want.as_str()));
}

/// F-200: 削除されたキーは移行先を示すエラーで起動しない（`-t`）。
#[test]
fn f200_removed_keys_are_rejected_with_migration_hint() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    for (line, hint) in [
        ("http = \"127.0.0.1:18080\"", "listen_cleartext"),
        ("tls_only = false", "listen_cleartext"),
        ("h2c_enabled = true", "listen_cleartext"),
        ("h2c_listen = \"127.0.0.1:18081\"", "listen_cleartext"),
        ("h2c_http1 = true", "listen_cleartext"),
        ("http3_enabled = true", "[http3]"),
    ] {
        std::fs::write(
            &cfg_path,
            format!("[server]\nlisten_cleartext = \"127.0.0.1:18080\"\nenable_secure_redirect = false\n{line}\n"),
        )
        .unwrap();
        let out = std::process::Command::new(veil_bin())
            .arg("-t")
            .arg("-c")
            .arg(&cfg_path)
            .output()
            .unwrap();
        assert!(!out.status.success(), "{line} must be rejected");
        let all = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(all.contains(hint), "{line}: no migration hint in: {all}");
    }
}

/// F-200: `[tls]` が要るのは `listen`（TLS）か HTTP/3 があるときだけ。
/// 平文だけなら省略でき、`listen` があって無ければ -t で失敗する。
#[test]
fn f188_missing_tls_is_rejected_unless_plaintext_only() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    let check = |cfg: &str| {
        std::fs::write(&cfg_path, cfg).unwrap();
        std::process::Command::new(veil_bin())
            .arg("-t")
            .arg("-c")
            .arg(&cfg_path)
            .output()
            .unwrap()
    };
    let out = check("[server]\nlisten = \"127.0.0.1:18443\"\n");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("TLS certificate is required"), "{}", stderr);

    let out =
        check("[server]\nlisten_cleartext = \"127.0.0.1:18443\"\nenable_secure_redirect = false\n");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// F-200: 規則違反（リスナー無し・同一アドレス・リダイレクト先無し・HTTP/3 に listen 無し・両方拒否）。
#[test]
fn f200_listener_rules_are_validated() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    let cfg_path = dir.path().join("config.toml");
    let tls = format!(
        "[tls]\ncert_path = \"{}\"\nkey_path = \"{}\"\n",
        tp(&cert),
        tp(&key)
    );
    let run = |cfg: String| {
        std::fs::write(&cfg_path, cfg).unwrap();
        std::process::Command::new(veil_bin())
            .arg("-t")
            .arg("-c")
            .arg(&cfg_path)
            .output()
            .unwrap()
    };
    // リスナー無し
    assert!(!run(format!("[server]\nthreads = 1\n{tls}"))
        .status
        .success());
    // listen と listen_cleartext が同じ
    assert!(!run(format!(
        "[server]\nlisten = \"127.0.0.1:18443\"\nlisten_cleartext = \"127.0.0.1:18443\"\nenable_secure_redirect = false\n{tls}"
    ))
    .status
    .success());
    // enable_secure_redirect（既定 true）なのに listen が無い
    assert!(!run(format!(
        "[server]\nlisten_cleartext = \"127.0.0.1:18080\"\n{tls}"
    ))
    .status
    .success());
    // allow_http / allow_h2c が両方 false
    assert!(!run(format!(
        "[server]\nlisten_cleartext = \"127.0.0.1:18080\"\nenable_secure_redirect = false\nallow_http = false\nallow_h2c = false\n{tls}"
    ))
    .status
    .success());
    // HTTP/3 は listen が要る
    assert!(!run(format!(
        "[server]\nlisten_cleartext = \"127.0.0.1:18080\"\nenable_secure_redirect = false\n[http3]\nenabled = true\n{tls}"
    ))
    .status
    .success());
    // listen + listen_cleartext + HTTP/3（listen の既定アドレス）は通る
    let ok = run(format!(
        "[server]\nlisten = \"127.0.0.1:18443\"\nlisten_cleartext = \"127.0.0.1:18080\"\n[http3]\nenabled = true\n{tls}"
    ));
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
}

// ===================== F-04 VDS =====================

/// 参照制御プレーン（tools/vds/vds_server.py、REST の長輪詢）からルートを配信し、反映・NACK・トークン認証を確かめる。
#[cfg(feature = "vds")]
#[test]
fn f04_vds_delivers_routes_and_keeps_previous_on_nack() {
    vds_scenario(VdsControlPlane::Python, "rest", false);
}

/// Rust の参照制御プレーン（tools/vds/server）の REST で同じ結果になる（F-199）。
#[cfg(feature = "vds")]
#[test]
fn f199_vds_rest_on_rust_control_plane() {
    vds_scenario(VdsControlPlane::Rust, "rest", false);
}

/// gRPC の双方向ストリーム（h2c）で同じ結果になる。制御プレーンの再起動から再接続する（F-199）。
#[cfg(feature = "vds")]
#[test]
fn f199_vds_grpc_stream_h2c() {
    vds_scenario(VdsControlPlane::Rust, "grpc", false);
}

/// gRPC の双方向ストリーム（TLS + ALPN h2、追加 CA、Bearer トークン）。
#[cfg(feature = "vds")]
#[test]
fn f199_vds_grpc_stream_tls() {
    vds_scenario(VdsControlPlane::Rust, "grpc", true);
}

#[cfg(feature = "vds")]
#[derive(Clone, Copy, PartialEq)]
enum VdsControlPlane {
    Python,
    Rust,
}

/// Rust の参照制御プレーンをビルドしてバイナリのパスを返す（テスト全体で 1 回）。
#[cfg(feature = "vds")]
fn vds_server_bin() -> std::path::PathBuf {
    static BUILT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    BUILT
        .get_or_init(|| {
            let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
            let target = root.join("target/examples-api");
            let st = std::process::Command::new(env!("CARGO"))
                .args(["build", "-q", "--manifest-path"])
                .arg(root.join("tools/vds/server/Cargo.toml"))
                .env("CARGO_TARGET_DIR", &target)
                .status()
                .expect("cargo");
            assert!(st.success(), "failed to build tools/vds/server");
            target.join(format!(
                "debug/veil-vds-server{}",
                std::env::consts::EXE_SUFFIX
            ))
        })
        .clone()
}

#[cfg(feature = "vds")]
fn spawn_vds_control_plane(
    kind: VdsControlPlane,
    snap: &std::path::Path,
    port: u16,
    token: &std::path::Path,
    tls: Option<(&std::path::Path, &std::path::Path)>,
) -> ChildGuard {
    let mut cmd = match kind {
        VdsControlPlane::Python => {
            // python3 の名前が違う環境（Windows の Store スタブ等）は PYTHON3 で指定する
            let py = std::env::var("PYTHON3").unwrap_or_else(|_| "python3".into());
            let mut c = std::process::Command::new(py);
            c.arg("-I")
                .arg(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tools/vds/vds_server.py"
                ))
                .args(["--watch-interval", "0.2"]);
            c
        }
        VdsControlPlane::Rust => std::process::Command::new(vds_server_bin()),
    };
    cmd.arg("--snapshot")
        .arg(snap)
        .arg("--listen")
        .arg(format!("127.0.0.1:{}", port))
        .arg("--token-file")
        .arg(token)
        .args(["--hold", "5"]);
    if let Some((cert, key)) = tls {
        cmd.arg("--tls-cert").arg(cert).arg("--tls-key").arg(key);
    }
    let child = cmd
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("failed to start the VDS control plane");
    let mut guard = ChildGuard(child);
    // 負荷の高いホスト（VM の同時実行・初回の .pyc 生成）では Python の起動に 15 秒を超えることがある。
    // 終了したらすぐ失敗にし、待つのは最大 60 秒。
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        if let Ok(Some(st)) = guard.0.try_wait() {
            panic!("control plane exited: {st}");
        }
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return guard;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "control plane did not start in 60s"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// 配信 → 更新 → NACK（前の版のまま）→ 修正、gRPC では制御プレーンの再起動からの再接続も確かめる。
#[cfg(feature = "vds")]
fn vds_scenario(kind: VdsControlPlane, transport: &str, tls: bool) {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in [("base", "base"), ("a", "A"), ("b", "B")] {
        let d = dir.path().join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("index.html"), body).unwrap();
    }
    let snap = dir.path().join("snapshot.toml");
    let route = |p: &std::path::Path| {
        format!(
            "[[route]]\n[route.conditions]\npath = \"/\"\n[route.action]\ntype = \"File\"\npath = \"{}\"\n",
            tp(p)
        )
    };
    std::fs::write(&snap, route(&dir.path().join("a"))).unwrap();
    let token = dir.path().join("token");
    std::fs::write(&token, "s3cret\n").unwrap();
    let certs = tls.then(|| write_self_signed(dir.path()));
    let cp_tls = certs.as_ref().map(|(c, k)| (c.as_path(), k.as_path()));

    let cp_port = free_port();
    let mut cp = spawn_vds_control_plane(kind, &snap, cp_port, &token, cp_tls);

    let port = free_port();
    let scheme = if tls { "https" } else { "http" };
    let ca = certs
        .as_ref()
        .map(|(c, _)| format!("ca_cert = \"{}\"\n", tp(c)))
        .unwrap_or_default();
    let config = format!(
        r#"
[server]
listen_cleartext = "127.0.0.1:{port}"
enable_secure_redirect = false
threads = 1
[logging]
level = "info"
[vds]
enabled = true
transport = "{transport}"
server = "{scheme}://127.0.0.1:{cp_port}"
node_id = "e2e"
token_file = "{token}"
poll_timeout_secs = 5
retry_initial_ms = 100
retry_max_ms = 1000
{ca}{base_route}"#,
        token = tp(&token),
        base_route = route(&dir.path().join("base")),
    );
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, config).unwrap();
    let veil = VeilProcess::spawn(&cfg_path, &[], &[]);
    assert!(
        wait_for_port(port, Duration::from_secs(20)),
        "{}",
        veil.log()
    );

    let body_is = |want: &'static str| {
        move || {
            http1_get(port, "/index.html")
                .map(|r| r.text() == want)
                .unwrap_or(false)
        }
    };
    let status = |cp_port: u16| -> String {
        if tls {
            // 状態は TLS の向こう（このテストのクライアントは平文のみ）なので見ない
            return String::new();
        }
        http1_get(cp_port, "/v1/status")
            .map(|r| r.text())
            .unwrap_or_default()
    };
    assert!(
        wait_until(Duration::from_secs(15), body_is("A")),
        "VDS snapshot A not applied ({}); log:\n{}",
        transport,
        veil.log()
    );

    // 配信内容を変える → 新しいリクエストに反映される
    std::fs::write(&snap, route(&dir.path().join("b"))).unwrap();
    assert!(
        wait_until(Duration::from_secs(15), body_is("B")),
        "VDS snapshot B not applied; log:\n{}",
        veil.log()
    );

    // 不正な版（存在しないディレクトリ）→ NACK され、前の版（B）のまま
    std::fs::write(&snap, route(&dir.path().join("missing"))).unwrap();
    let nacked = wait_until(Duration::from_secs(15), || {
        if tls {
            veil.log().contains("rejected snapshot")
        } else {
            let s = status(cp_port);
            s.contains("\"nacks\": 1") || s.contains("\"nacks\":1")
        }
    });
    assert!(nacked, "NACK not reported; log:\n{}", veil.log());
    assert!(body_is("B")(), "previous snapshot must stay after NACK");

    // 直すと再び反映される
    std::fs::write(&snap, route(&dir.path().join("a"))).unwrap();
    assert!(
        wait_until(Duration::from_secs(15), body_is("A")),
        "{}",
        veil.log()
    );
    if kind == VdsControlPlane::Rust && !tls {
        let s = status(cp_port);
        assert!(
            s.contains(&format!("\"transport\":\"{}\"", transport)),
            "status: {}",
            s
        );
    }

    if transport == "grpc" {
        // 制御プレーンを止めている間に内容を変え、起動し直すと再接続して届く
        drop(cp);
        std::fs::write(&snap, route(&dir.path().join("b"))).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            body_is("A")(),
            "must keep the last snapshot while disconnected"
        );
        cp = spawn_vds_control_plane(kind, &snap, cp_port, &token, cp_tls);
        assert!(
            wait_until(Duration::from_secs(20), body_is("B")),
            "not re-applied after the control plane restarted; log:\n{}",
            veil.log()
        );
    }
    drop(cp);
}

/// F-211: 制御プレーンの冗長化（1 台目を止めると 2 台目から配信が続く）と、状態のメトリクス・管理 API。
#[cfg(feature = "vds")]
#[test]
fn f211_vds_failover_status_metrics_admin() {
    for transport in ["grpc", "rest"] {
        vds_failover(transport);
    }
}

#[cfg(feature = "vds")]
fn vds_failover(transport: &str) {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in [("base", "base"), ("a", "A"), ("b", "B")] {
        let d = dir.path().join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("index.html"), body).unwrap();
    }
    let route = |p: &std::path::Path| {
        format!(
            "[[route]]\n[route.conditions]\npath = \"/\"\n[route.action]\ntype = \"File\"\npath = \"{}\"\n",
            tp(p)
        )
    };
    // 2 台の制御プレーンは別の内容を配る（どちらから届いたかが本文でわかる）
    let snap1 = dir.path().join("snap1.toml");
    let snap2 = dir.path().join("snap2.toml");
    std::fs::write(&snap1, route(&dir.path().join("a"))).unwrap();
    std::fs::write(&snap2, route(&dir.path().join("b"))).unwrap();
    let token = dir.path().join("token");
    std::fs::write(&token, "s3cret\n").unwrap();
    let (p1, p2) = (free_port(), free_port());
    let cp1 = spawn_vds_control_plane(VdsControlPlane::Rust, &snap1, p1, &token, None);
    let cp2 = spawn_vds_control_plane(VdsControlPlane::Rust, &snap2, p2, &token, None);

    let port = free_port();
    let config = format!(
        r#"
[server]
listen_cleartext = "127.0.0.1:{port}"
enable_secure_redirect = false
threads = 1
[logging]
level = "info"
[admin]
enabled = true
secret = "adm"
allowed_ips = ["127.0.0.1"]
[prometheus]
enabled = true
path = "/metrics"
allowed_ips = ["127.0.0.1"]
[vds]
enabled = true
transport = "{transport}"
servers = ["http://127.0.0.1:{p1}", "http://127.0.0.1:{p2}"]
node_id = "e2e"
token_file = "{token}"
poll_timeout_secs = 2
retry_initial_ms = 100
retry_max_ms = 500
{base_route}"#,
        token = tp(&token),
        base_route = route(&dir.path().join("base")),
    );
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, config).unwrap();
    let veil = VeilProcess::spawn(&cfg_path, &[], &[]);
    assert!(
        wait_for_port(port, Duration::from_secs(20)),
        "{}",
        veil.log()
    );
    let body_is = |want: &'static str| {
        move || {
            http1_get(port, "/index.html")
                .map(|r| r.text() == want)
                .unwrap_or(false)
        }
    };
    let admin_vds = || {
        http1_request(
            port,
            "GET",
            "/__admin/vds",
            &[("Authorization", "Bearer adm")],
            b"",
        )
        .map(|r| r.text())
        .unwrap_or_default()
    };
    assert!(
        wait_until(Duration::from_secs(15), body_is("A")),
        "{}: snapshot from the first control plane not applied; log:\n{}",
        transport,
        veil.log()
    );
    let s = admin_vds();
    assert!(
        s.contains("\"enabled\":true")
            && s.contains("\"connected\":true")
            && s.contains(&format!("127.0.0.1:{}", p1))
            && s.contains("\"updates_acked\":1"),
        "{}: /__admin/vds: {}",
        transport,
        s
    );
    // 認証が要る
    let r = http1_get(port, "/__admin/vds").unwrap();
    assert_eq!(r.status, 401, "{}", r.text());

    // 1 台目を止める → 2 台目から配信が続く
    drop(cp1);
    assert!(
        wait_until(Duration::from_secs(20), body_is("B")),
        "{}: did not fail over to the second control plane; log:\n{}",
        transport,
        veil.log()
    );
    let s = admin_vds();
    assert!(
        s.contains(&format!("127.0.0.1:{}", p2)) && s.contains("\"updates_acked\":2"),
        "{}: /__admin/vds after failover: {}",
        transport,
        s
    );
    let m = http1_get(port, "/metrics")
        .map(|r| r.text())
        .unwrap_or_default();
    for want in [
        "veil_proxy_vds_connected 1",
        "veil_proxy_vds_updates_total{result=\"applied\"} 2",
        "veil_proxy_vds_updates_total{result=\"rejected\"} 0",
        "veil_proxy_vds_applied_info{version_info=",
        "veil_proxy_vds_last_update_timestamp_seconds ",
    ] {
        assert!(
            m.contains(want),
            "{}: metrics lack {:?}:\n{}",
            transport,
            want,
            m
        );
    }
    drop(cp2);
}

/// F-211: `initial_fetch_timeout_ms` の間は最初のスナップショットが届くまでリスナーを開かない。
/// 期限を過ぎたらファイルの設定で開始する。
#[cfg(feature = "vds")]
#[test]
fn f211_vds_initial_fetch_wait() {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in [("base", "base"), ("a", "A")] {
        let d = dir.path().join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("index.html"), body).unwrap();
    }
    let route = |p: &std::path::Path| {
        format!(
            "[[route]]\n[route.conditions]\npath = \"/\"\n[route.action]\ntype = \"File\"\npath = \"{}\"\n",
            tp(p)
        )
    };
    let snap = dir.path().join("snap.toml");
    std::fs::write(&snap, route(&dir.path().join("a"))).unwrap();
    let token = dir.path().join("token");
    std::fs::write(&token, "s3cret\n").unwrap();
    let write_config = |name: &str, port: u16, cp_port: u16, wait_ms: u64| {
        let config = format!(
            r#"
[server]
listen_cleartext = "127.0.0.1:{port}"
enable_secure_redirect = false
threads = 1
[logging]
level = "info"
[vds]
enabled = true
server = "http://127.0.0.1:{cp_port}"
node_id = "e2e"
token_file = "{token}"
retry_initial_ms = 100
retry_max_ms = 300
initial_fetch_timeout_ms = {wait_ms}
{base_route}"#,
            token = tp(&token),
            base_route = route(&dir.path().join("base")),
        );
        let p = dir.path().join(name);
        std::fs::write(&p, config).unwrap();
        p
    };

    // 1. 制御プレーンが後から起動する: それまでリスナーは開かず、開いたときには VDS の内容
    let (port, cp_port) = (free_port(), free_port());
    let cfg = write_config("wait.toml", port, cp_port, 60_000);
    let veil = VeilProcess::spawn(&cfg, &[], &[]);
    assert!(
        !wait_for_port(port, Duration::from_secs(2)),
        "listener opened before the initial snapshot; log:\n{}",
        veil.log()
    );
    let cp = spawn_vds_control_plane(VdsControlPlane::Rust, &snap, cp_port, &token, None);
    assert!(
        wait_for_port(port, Duration::from_secs(20)),
        "listener did not open after the initial snapshot; log:\n{}",
        veil.log()
    );
    let r = http1_get(port, "/index.html").unwrap();
    assert_eq!(
        r.text(),
        "A",
        "the first response must use the VDS snapshot"
    );
    drop(veil);
    drop(cp);

    // 2. 制御プレーンが無い: 期限でファイルの設定のまま開始する
    let (port, cp_port) = (free_port(), free_port());
    let cfg = write_config("timeout.toml", port, cp_port, 1_500);
    let start = std::time::Instant::now();
    let veil = VeilProcess::spawn(&cfg, &[], &[]);
    assert!(
        wait_for_port(port, Duration::from_secs(20)),
        "listener did not open after initial_fetch_timeout_ms; log:\n{}",
        veil.log()
    );
    assert!(
        start.elapsed() >= Duration::from_millis(1_400),
        "listener opened before initial_fetch_timeout_ms"
    );
    assert_eq!(http1_get(port, "/index.html").unwrap().text(), "base");
    assert!(
        veil.log().contains("initial_fetch_timeout_ms"),
        "{}",
        veil.log()
    );
}

/// 子プロセスを Drop で止める（VDS の制御プレーン用）。
#[cfg(feature = "vds")]
struct ChildGuard(std::process::Child);

#[cfg(feature = "vds")]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// ===================== F-217（TOML の mTLS） =====================

/// テスト用の CA と、それが発行するサーバ証明書（URI SAN 付き）・クライアント証明書を書き出す。
fn write_mtls_pki(dir: &std::path::Path) {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair, SanType};
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = ca_params.self_signed(&ca_key).unwrap();
    std::fs::write(dir.join("ca.pem"), ca.pem()).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);

    let server_key = KeyPair::generate().unwrap();
    let mut sp = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    sp.subject_alt_names
        .push(SanType::IpAddress("127.0.0.1".parse().unwrap()));
    sp.subject_alt_names
        .push(SanType::URI("spiffe://test/server".try_into().unwrap()));
    let server = sp.signed_by(&server_key, &issuer).unwrap();
    std::fs::write(dir.join("server.pem"), server.pem()).unwrap();
    std::fs::write(dir.join("server.key"), server_key.serialize_pem()).unwrap();

    let client_key = KeyPair::generate().unwrap();
    let cp = CertificateParams::new(vec!["client.test".to_string()]).unwrap();
    let client = cp.signed_by(&client_key, &issuer).unwrap();
    std::fs::write(dir.join("client.pem"), client.pem()).unwrap();
    std::fs::write(dir.join("client.key"), client_key.serialize_pem()).unwrap();

    // 同じ CA が発行した別の ID（F-220 の SAN の許可リストの試験用）
    let other_key = KeyPair::generate().unwrap();
    let op = CertificateParams::new(vec!["other.test".to_string()]).unwrap();
    let other = op.signed_by(&other_key, &issuer).unwrap();
    std::fs::write(dir.join("other.pem"), other.pem()).unwrap();
    std::fs::write(dir.join("other.key"), other_key.serialize_pem()).unwrap();
}

/// TLS で GET して応答の先頭行を返す（サーバ証明書は検証しない。`client` はクライアント証明書の PEM の組）。
fn tls_get_status_line(
    port: u16,
    path: &str,
    client: Option<(&std::path::Path, &std::path::Path)>,
) -> std::io::Result<String> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use std::io::{Read, Write};
    use std::sync::Arc;
    let _ = veil::tls_provider::provider::default_provider().install_default();
    let b = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerifyServer));
    let config = match client {
        Some((cert, key)) => {
            let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
                .unwrap()
                .map(|c| c.unwrap())
                .collect();
            let key = PrivateKeyDer::from_pem_file(key).unwrap();
            b.with_client_auth_cert(certs, key).unwrap()
        }
        None => b.with_no_client_auth(),
    };
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let conn = rustls::ClientConnection::new(Arc::new(config), name)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let sock = connect_local(port, Duration::from_secs(2))?;
    sock.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut tls = rustls::StreamOwned::new(conn, sock);
    write!(
        tls,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )?;
    tls.flush()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match tls.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if buf.is_empty() => return Err(e),
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&buf).to_string();
    Ok(text.lines().next().unwrap_or("").to_string())
}

#[derive(Debug)]
struct NoVerifyServer;

impl rustls::client::danger::ServerCertVerifier for NoVerifyServer {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        veil::tls_provider::provider::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// `[tls] client_auth = "require"` のサーバは証明書の無いクライアントを拒否し、`client_ca` が発行した証明書なら受ける。
/// その前段の veil は `tls_client_cert`・`tls_client_key`・`tls_ca`・`expected_san`（URI SAN）で上流へ mTLS で
/// 接続でき、`expected_san` が合わない上流は 502 になる。
#[test]
fn f217_toml_mtls_server_client_auth_and_upstream_client_cert() {
    let dir = tempfile::tempdir().unwrap();
    let pki = dir.path();
    write_mtls_pki(pki);
    let www = pki.join("www");
    std::fs::create_dir_all(&www).unwrap();
    std::fs::write(www.join("index.html"), "hello").unwrap();
    std::fs::write(www.join("ok"), "hello").unwrap();
    std::fs::write(www.join("bad"), "hello").unwrap();

    // 受け側: mTLS 必須の TLS サーバ
    let up_port = free_port();
    let up_dir = pki.join("up");
    std::fs::create_dir_all(&up_dir).unwrap();
    let up_cfg = format!(
        r#"
[server]
listen = "127.0.0.1:{up_port}"
threads = 1
[tls]
cert_path = "{p}/server.pem"
key_path = "{p}/server.key"
client_auth = "require"
client_ca = "{p}/ca.pem"
[logging]
level = "warn"
[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "{www}"
"#,
        p = tp(pki),
        www = tp(&www),
    );
    std::fs::write(up_dir.join("config.toml"), up_cfg).unwrap();
    let mut up = VeilProcess::spawn(&up_dir.join("config.toml"), &[], &[]);
    assert!(
        wait_for_port(up_port, Duration::from_secs(20)),
        "{}",
        up.log()
    );

    let no_cert = tls_get_status_line(up_port, "/ok", None);
    assert!(
        !matches!(&no_cert, Ok(l) if l.starts_with("HTTP/1.1 200")),
        "client without a certificate must be rejected: {:?}",
        no_cert
    );
    let with_cert = tls_get_status_line(
        up_port,
        "/ok",
        Some((&pki.join("client.pem"), &pki.join("client.key"))),
    )
    .unwrap_or_else(|e| panic!("mTLS request failed: {e}\n{}", up.log()));
    assert!(with_cert.starts_with("HTTP/1.1 200"), "{with_cert}");

    // 送り側: 平文で受けて上流へ mTLS
    let front_port = free_port();
    let front_dir = pki.join("front");
    std::fs::create_dir_all(&front_dir).unwrap();
    let front_cfg = format!(
        r#"
[server]
listen_cleartext = "127.0.0.1:{front_port}"
enable_secure_redirect = false
threads = 1
[logging]
level = "warn"
[[route]]
[route.conditions]
path = "/ok"
[route.action]
type = "Proxy"
url = "https://127.0.0.1:{up_port}"
tls_client_cert = "{p}/client.pem"
tls_client_key = "{p}/client.key"
tls_ca = "{p}/ca.pem"
expected_san = ["spiffe://test/server"]
[[route]]
[route.conditions]
path = "/bad"
[route.action]
type = "Proxy"
url = "https://127.0.0.1:{up_port}"
tls_client_cert = "{p}/client.pem"
tls_client_key = "{p}/client.key"
tls_ca = "{p}/ca.pem"
expected_san = ["spiffe://test/other"]
"#,
        p = tp(pki),
    );
    std::fs::write(front_dir.join("config.toml"), front_cfg).unwrap();
    let mut front = VeilProcess::spawn(&front_dir.join("config.toml"), &[], &[]);
    assert!(
        wait_for_port(front_port, Duration::from_secs(20)),
        "{}",
        front.log()
    );

    let ok = http1_get(front_port, "/ok").expect("front /ok");
    if ok.status != 200 {
        front.terminate(Duration::from_secs(5));
        up.terminate(Duration::from_secs(5));
        panic!(
            "proxy over mTLS failed: {}\nfront:\n{}\nupstream:\n{}",
            ok.status,
            front.log(),
            up.log()
        );
    }
    assert_eq!(ok.text(), "hello");
    let bad = http1_get(front_port, "/bad").expect("front /bad");
    assert_eq!(bad.status, 502, "upstream with a different SAN must fail");
}

// ===================== F-219（TOML のルート単位のトラフィック制御） =====================

/// 固定応答（`type = "Respond"`）・重み付き分割・フォルトの中断・CORS のプリフライトを、HTTP/1.1 と h2c で確かめる。
/// 分割先は同じ veil の別ルート（`/a`・`/b` の固定応答）。
#[test]
fn f219_route_traffic_respond_split_fault_cors() {
    let dir = tempfile::tempdir().unwrap();
    let port = free_port();
    let config = format!(
        r#"
[server]
listen_cleartext = "127.0.0.1:{port}"
enable_secure_redirect = false
allow_h2c = true
threads = 1
[logging]
level = "warn"

[[route]]
[route.conditions]
path = "/a"
[route.action]
type = "Respond"
body = "A"

[[route]]
[route.conditions]
path = "/b"
[route.action]
type = "Respond"
body = "B"

[[route]]
[route.conditions]
path = "/direct"
[route.action]
type = "Respond"
status = 418
body = "teapot"
content_type = "text/plain"

[[route]]
[route.conditions]
path = "/split"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{port}/a"
[route.traffic]
split = [
  {{ url = "http://127.0.0.1:{port}/a", weight = 80 }},
  {{ url = "http://127.0.0.1:{port}/b", weight = 20 }},
]

[[route]]
[route.conditions]
path = "/fault"
[route.action]
type = "Respond"
body = "not aborted"
[route.traffic.fault]
abort_status = 503
abort_percent = 100

[[route]]
[route.conditions]
path = "/cors"
[route.action]
type = "Respond"
body = "actual"
[route.traffic.cors]
allow_origins = ["https://a.example"]
allow_methods = ["GET", "POST"]
max_age_secs = 600
"#
    );
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, config).unwrap();
    let veil = VeilProcess::spawn(&cfg_path, &[], &[]);
    assert!(
        wait_for_port(port, Duration::from_secs(20)),
        "{}",
        veil.log()
    );

    // 固定応答
    let r = http1_get(port, "/direct").unwrap();
    assert_eq!(r.status, 418);
    assert_eq!(r.text(), "teapot");
    assert_eq!(r.header("content-type"), Some("text/plain"));
    let (st, body) = h2c_get(port, "/direct");
    assert_eq!((st, body.as_slice()), (418, &b"teapot"[..]));

    // 重み付き分割（80:20）
    let mut a = 0;
    let n = 200;
    for _ in 0..n {
        let r = http1_get(port, "/split").unwrap();
        assert_eq!(r.status, 200, "{}", veil.log());
        match r.text().as_str() {
            "A" => a += 1,
            "B" => {}
            other => panic!("unexpected body {other:?}"),
        }
    }
    assert!((130..=190).contains(&a), "A={a} of {n}");
    let (st, _) = h2c_get(port, "/split");
    assert_eq!(st, 200);

    // フォルトの中断（100%）。h2c はストリーミングの下見を通る経路でも 1 回だけ判定する
    assert_eq!(http1_get(port, "/fault").unwrap().status, 503);
    let (st, body) = h2c_get(port, "/fault");
    assert_eq!((st, body.as_slice()), (503, &b"fault filter abort"[..]));

    // CORS のプリフライト（一覧のオリジンだけ）。プリフライトでない要求は action のまま
    let pre = http1_request(
        port,
        "OPTIONS",
        "/cors",
        &[
            ("Origin", "https://a.example"),
            ("Access-Control-Request-Method", "POST"),
        ],
        b"",
    )
    .unwrap();
    assert_eq!(pre.status, 200);
    assert_eq!(
        pre.header("access-control-allow-origin"),
        Some("https://a.example")
    );
    assert_eq!(pre.header("access-control-allow-methods"), Some("GET,POST"));
    assert_eq!(pre.header("access-control-max-age"), Some("600"));
    let other = http1_request(
        port,
        "OPTIONS",
        "/cors",
        &[
            ("Origin", "https://evil.example"),
            ("Access-Control-Request-Method", "POST"),
        ],
        b"",
    )
    .unwrap();
    assert!(other.header("access-control-allow-origin").is_none());
    assert_eq!(http1_get(port, "/cors").unwrap().text(), "actual");
}

// ===================== F-220（TOML の JWT・ID による認可） =====================

/// テスト用: ES256 の鍵で payload に署名したトークンと、その JWKS。
#[cfg(all(feature = "jwt", not(any(target_os = "openbsd", target_os = "netbsd"))))]
fn es256_token(payload: &str) -> (String, String) {
    use aws_lc_rs::rand::SystemRandom;
    use aws_lc_rs::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};
    fn enc(b: &[u8]) -> String {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for ch in b.chunks(3) {
            let n = (ch[0] as u32) << 16
                | (*ch.get(1).unwrap_or(&0) as u32) << 8
                | *ch.get(2).unwrap_or(&0) as u32;
            for i in 0..(ch.len() + 1) {
                out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            }
        }
        out
    }
    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
    let kp = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref()).unwrap();
    let pt = kp.public_key().as_ref();
    let jwks = format!(
        r#"{{"keys":[{{"kty":"EC","crv":"P-256","kid":"k1","x":"{}","y":"{}"}}]}}"#,
        enc(&pt[1..33]),
        enc(&pt[33..65])
    );
    let input = format!(
        "{}.{}",
        enc(br#"{"alg":"ES256","kid":"k1","typ":"JWT"}"#),
        enc(payload.as_bytes())
    );
    let sig = kp.sign(&rng, input.as_bytes()).unwrap();
    (format!("{}.{}", input, enc(sig.as_ref())), jwks)
}

/// `[route.jwt]`: トークンの無い要求・検証できない要求は 401、claim の条件に合わない要求は 403、正しい要求は
/// action へ（HTTP/1.1・h2c）。`[tls] client_allowed_sans`: 同じ CA の証明書でも一覧に無い ID はハンドシェイクで拒否。
#[cfg(all(feature = "jwt", not(any(target_os = "openbsd", target_os = "netbsd"))))]
#[test]
fn f220_route_jwt_and_client_allowed_sans() {
    let dir = tempfile::tempdir().unwrap();
    let (good, jwks) = es256_token(
        r#"{"iss":"https://issuer.test","aud":"api","exp":4000000000,"sub":"alice","scope":"read write"}"#,
    );
    // 同じ鍵の別のトークンは作れないので、claim 違いは別の鍵で作り JWKS を 2 本にする
    let (no_scope, jwks2) = es256_token(
        r#"{"iss":"https://issuer.test","aud":"api","exp":4000000000,"sub":"bob","scope":"write"}"#,
    );
    let k1: serde_json::Value = serde_json::from_str(&jwks).unwrap();
    let mut k2: serde_json::Value = serde_json::from_str(&jwks2).unwrap();
    k2["keys"][0]["kid"] = serde_json::Value::String("k1".into());
    let both = serde_json::json!({"keys": [k1["keys"][0].clone(), k2["keys"][0].clone()]});
    let jwks_path = dir.path().join("jwks.json");
    std::fs::write(&jwks_path, both.to_string()).unwrap();
    let (other_iss, _) = es256_token(r#"{"iss":"https://evil.test","aud":"api"}"#);

    let port = free_port();
    let config = format!(
        r#"
[server]
listen_cleartext = "127.0.0.1:{port}"
enable_secure_redirect = false
allow_h2c = true
threads = 1
[logging]
level = "warn"

[[route]]
[route.conditions]
path = "/api"
[route.action]
type = "Respond"
body = "secret"
[route.jwt]
issuer = "https://issuer.test"
audiences = ["api"]
jwks_path = "{jwks}"
require_claims = {{ scope = ["read"] }}
"#,
        jwks = tp(&jwks_path)
    );
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, config).unwrap();
    let veil = VeilProcess::spawn(&cfg_path, &[], &[]);
    assert!(
        wait_for_port(port, Duration::from_secs(20)),
        "{}",
        veil.log()
    );

    let get = |auth: Option<String>| {
        let h: Vec<(&str, String)> = auth.map(|a| vec![("Authorization", a)]).unwrap_or_default();
        let hr: Vec<(&str, &str)> = h.iter().map(|(k, v)| (*k, v.as_str())).collect();
        http1_request(port, "GET", "/api", &hr, b"").unwrap()
    };
    let r = get(None);
    assert_eq!(r.status, 401, "{}", veil.log());
    assert!(r.header("www-authenticate").is_some());
    assert_eq!(get(Some("Bearer garbage".into())).status, 401);
    assert_eq!(get(Some(format!("Bearer {other_iss}"))).status, 401);
    assert_eq!(get(Some(format!("Bearer {no_scope}"))).status, 403);
    let ok = get(Some(format!("Bearer {good}")));
    assert_eq!((ok.status, ok.text().as_str()), (200, "secret"));
    // h2c（ストリーミングの下見を通る経路）でもトークンが無ければ 401
    let (st, _) = h2c_get(port, "/api");
    assert_eq!(st, 401);

    // ID による認可（ハンドシェイク）
    let pki = dir.path().join("pki");
    std::fs::create_dir_all(&pki).unwrap();
    write_mtls_pki(&pki);
    let www = pki.join("www");
    std::fs::create_dir_all(&www).unwrap();
    std::fs::write(www.join("index.html"), "hello").unwrap();
    let tls_port = free_port();
    let tls_cfg = format!(
        r#"
[server]
listen = "127.0.0.1:{tls_port}"
threads = 1
[tls]
cert_path = "{p}/server.pem"
key_path = "{p}/server.key"
client_auth = "require"
client_ca = "{p}/ca.pem"
client_allowed_sans = ["client.test"]
[logging]
level = "warn"
[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "{www}"
"#,
        p = tp(&pki),
        www = tp(&www)
    );
    let tls_cfg_path = pki.join("config.toml");
    std::fs::write(&tls_cfg_path, tls_cfg).unwrap();
    let tls_veil = VeilProcess::spawn(&tls_cfg_path, &[], &[]);
    assert!(
        wait_for_port(tls_port, Duration::from_secs(20)),
        "{}",
        tls_veil.log()
    );
    let allowed = tls_get_status_line(
        tls_port,
        "/",
        Some((&pki.join("client.pem"), &pki.join("client.key"))),
    )
    .unwrap_or_else(|e| panic!("allowed identity rejected: {e}\n{}", tls_veil.log()));
    assert!(allowed.starts_with("HTTP/1.1 200"), "{allowed}");
    let denied = tls_get_status_line(
        tls_port,
        "/",
        Some((&pki.join("other.pem"), &pki.join("other.key"))),
    );
    assert!(
        !matches!(&denied, Ok(l) if l.starts_with("HTTP/1.1 200")),
        "identity outside client_allowed_sans must be rejected: {denied:?}"
    );
}

// ===================== F-234 / B-132 =====================

/// HTTP/3 で要求を 1 つ送り、(状態コード, 本文) を返す（F-234 のプロトコルの行列の試験）。
#[cfg(feature = "http3")]
fn h3_request(port: u16, method: &str, path: &str, body: &[u8]) -> (u16, Vec<u8>) {
    use bytes::Buf;
    use std::sync::Arc;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let _ = veil::tls_provider::provider::default_provider().install_default();
        let mut tls = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifyServer))
            .with_no_client_auth();
        tls.alpn_protocols = vec![b"h3".to_vec()];
        let qc = quinn::crypto::rustls::QuicClientConfig::try_from(Arc::new(tls)).unwrap();
        let mut ep = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        ep.set_default_client_config(quinn::ClientConfig::new(Arc::new(qc)));
        let conn = ep
            .connect(([127, 0, 0, 1], port).into(), "localhost")
            .unwrap()
            .await
            .unwrap();
        let (mut driver, mut send) = h3::client::new(h3_quinn::Connection::new(conn))
            .await
            .unwrap();
        let drive = tokio::spawn(async move {
            let _ = driver.wait_idle().await;
        });
        let req = http::Request::builder()
            .method(method)
            .uri(format!("https://localhost:{port}{path}"))
            .body(())
            .unwrap();
        let mut stream = send.send_request(req).await.unwrap();
        if !body.is_empty() {
            stream
                .send_data(bytes::Bytes::copy_from_slice(body))
                .await
                .unwrap();
        }
        stream.finish().await.unwrap();
        let resp = stream.recv_response().await.unwrap();
        let mut out = Vec::new();
        while let Some(mut chunk) = stream.recv_data().await.unwrap() {
            out.extend_from_slice(chunk.chunk());
            let n = chunk.remaining();
            chunk.advance(n);
        }
        drop(send);
        drive.abort();
        ep.close(0u32.into(), b"");
        (resp.status().as_u16(), out)
    })
}

/// 同じ設定に対して HTTP/1.1・h2c・HTTP/3 が同じ判定をする（F-234 の共通の段）。
/// B-132: 本文の大きさの上限を超えた要求は 413 で、レート制限の枠を使わない（3 経路とも）。
#[test]
fn f234_protocol_matrix_same_decisions() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    let port = free_port();
    let tls_port = free_port();
    let mut routes = String::new();
    for p in ["h1", "h2", "h3"] {
        routes.push_str(&format!(
            r#"
[[route]]
[route.conditions]
path = "/big-{p}"
[route.action]
type = "Respond"
body = "small"
[route.security]
max_request_body_size = 4
rate_limit_requests_per_min = 1
"#
        ));
    }
    let config = format!(
        r#"
[server]
listen = "127.0.0.1:{tls_port}"
listen_cleartext = "127.0.0.1:{port}"
enable_secure_redirect = false
allow_h2c = true
threads = 1
[logging]
level = "warn"
[tls]
cert_path = "{cert}"
key_path = "{key}"
[http3]
enabled = true

[[route]]
[route.conditions]
path = "/ok"
[route.action]
type = "Respond"
body = "ok"

[[route]]
[route.conditions]
path = "/deny"
[route.action]
type = "Respond"
body = "never"
[route.security]
allowed_ips = ["10.0.0.0/8"]

[[route]]
[route.conditions]
path = "/post-only"
[route.action]
type = "Respond"
body = "posted"
[route.security]
allowed_methods = ["POST"]
{routes}"#,
        cert = tp(&cert),
        key = tp(&key),
    );
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, config).unwrap();
    let veil = VeilProcess::spawn(&cfg_path, &[], &[]);
    assert!(
        wait_for_port(port, Duration::from_secs(20))
            && wait_for_port(tls_port, Duration::from_secs(20)),
        "{}",
        veil.log()
    );

    // (経路, 要求) → 状態コード。3 経路で同じでなければならない
    let h1 = |m: &str, p: &str, body: &[u8]| http1_request(port, m, p, &[], body).unwrap().status;
    let h2 = |m: &str, p: &str, body: &[u8]| -> u16 {
        if m == "GET" {
            h2c_get(port, p).0
        } else {
            h2c_post(port, p, body.len(), Duration::from_secs(10)).unwrap_or(0)
        }
    };
    let cases: &[(&str, &str, &[u8], u16)] = &[
        ("GET", "/ok", b"", 200),
        ("GET", "/no-such-route", b"", 404),
        ("GET", "/deny", b"", 403),
        ("GET", "/post-only", b"", 405),
        ("POST", "/post-only", b"x", 200),
    ];
    for &(m, p, body, want) in cases {
        assert_eq!(h1(m, p, body), want, "HTTP/1.1 {m} {p}\n{}", veil.log());
        assert_eq!(h2(m, p, body), want, "h2c {m} {p}\n{}", veil.log());
        #[cfg(feature = "http3")]
        assert_eq!(
            h3_request(tls_port, m, p, body).0,
            want,
            "HTTP/3 {m} {p}\n{}",
            veil.log()
        );
    }

    // 上限超過は何回送っても 413（枠を使わない）→ 小さい要求は 1 回だけ通り、次は 429
    let check_order = |name: &str, send: &dyn Fn(&str, &[u8]) -> u16| {
        let path = format!("/big-{name}");
        for _ in 0..3 {
            assert_eq!(
                send(&path, b"0123456789"),
                413,
                "{name}: oversized\n{}",
                veil.log()
            );
        }
        assert_eq!(
            send(&path, b"ok"),
            200,
            "{name}: first small\n{}",
            veil.log()
        );
        assert_eq!(
            send(&path, b"ok"),
            429,
            "{name}: second small\n{}",
            veil.log()
        );
    };
    check_order("h1", &|p, b| h1("POST", p, b));
    check_order("h2", &|p, b| h2("POST", p, b));
    #[cfg(feature = "http3")]
    check_order("h3", &|p, b| h3_request(tls_port, "POST", p, b).0);
}

// ===================== F-260 =====================

/// HttpArena と同じ 4 ポート（平文 HTTP/1.1 + h2c・TLS で h2 を選ばない HTTP/1.1・h2c だけ・TLS h2 + HTTP/3）と
/// https へのリダイレクトのリスナーを 1 プロセスの `[[listener]]` で立てる。
fn f260_config(dir: &std::path::Path, ports: &F260Ports, plain_extra: &str) -> String {
    let (cert, key) = write_self_signed(dir);
    let www = dir.join("www");
    std::fs::create_dir_all(&www).unwrap();
    std::fs::write(www.join("index.html"), "multi").unwrap();
    let http3 = if cfg!(feature = "http3") {
        "http3 = true"
    } else {
        ""
    };
    format!(
        r#"
[server]
http2_enabled = true
threads = 1
[tls]
cert_path = "{cert}"
key_path = "{key}"
ktls_enabled = false
[logging]
level = "info"

[[listener]]
name = "plain"
listen = "127.0.0.1:{plain}"
tls = false

[[listener]]
name = "h1tls"
listen = "127.0.0.1:{h1tls}"
tls = true
http2 = false

[[listener]]
name = "h2c"
listen = "127.0.0.1:{h2c}"
tls = false
{plain_extra}

[[listener]]
name = "https"
listen = "127.0.0.1:{https}"
tls = true
{http3}

[[listener]]
name = "redirect"
listen = "127.0.0.1:{redirect}"
tls = false
secure_redirect = true
redirect_to = "https"

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "{www}"
"#,
        cert = tp(&cert),
        key = tp(&key),
        www = tp(&www),
        plain = ports.plain,
        h1tls = ports.h1tls,
        h2c = ports.h2c,
        https = ports.https,
        redirect = ports.redirect,
    )
}

struct F260Ports {
    plain: u16,
    h1tls: u16,
    h2c: u16,
    https: u16,
    redirect: u16,
}

impl F260Ports {
    fn new() -> Self {
        F260Ports {
            plain: free_port(),
            h1tls: free_port(),
            h2c: free_port(),
            https: free_port(),
            redirect: free_port(),
        }
    }

    fn all(&self) -> [u16; 5] {
        [self.plain, self.h1tls, self.h2c, self.https, self.redirect]
    }
}

/// F-260: 複数の `[[listener]]` を 1 プロセスで待ち受け、リスナーごとのプロトコル・ALPN・リダイレクトを守る。
#[test]
fn f260_multiple_listeners_in_one_process() {
    let dir = tempfile::tempdir().unwrap();
    let ports = F260Ports::new();
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(
        &cfg_path,
        f260_config(dir.path(), &ports, "allow_http = false"),
    )
    .unwrap();
    let veil = VeilProcess::spawn(&cfg_path, &[], &[]);
    for p in ports.all() {
        assert!(
            wait_for_port(p, Duration::from_secs(20)),
            "port {p}: {}",
            veil.log()
        );
    }
    // 1 リスナー 1 行の起動ログ
    for name in ["plain", "h1tls", "h2c", "https", "redirect"] {
        assert!(
            veil.log().contains(&format!("listener \"{name}\"")),
            "{name}: {}",
            veil.log()
        );
    }

    // 平文: HTTP/1.1 と h2c
    let resp = http1_get(ports.plain, "/index.html").expect("plain HTTP/1.1");
    assert_eq!(resp.status, 200, "{}", veil.log());
    assert_eq!(resp.text(), "multi");
    assert_eq!(
        h2c_get(ports.plain, "/index.html"),
        (200, b"multi".to_vec())
    );

    // TLS（http2 = false）: h2 を出しても選ばれず HTTP/1.1
    let (alpn, resp) =
        tls_alpn_get(ports.h1tls, &[b"h2", b"http/1.1"], "/index.html").expect("TLS h1");
    assert_ne!(alpn.as_deref(), Some(&b"h2"[..]), "{}", veil.log());
    let resp = resp.expect("HTTP/1.1 over TLS");
    assert_eq!(resp.status, 200, "{}", veil.log());
    assert_eq!(resp.text(), "multi");

    // h2c だけ: HTTP/1.1 は 426、h2c は 200
    let resp = http1_get(ports.h2c, "/index.html").expect("426");
    assert_eq!(resp.status, 426, "{}", veil.log());
    assert_eq!(h2c_get(ports.h2c, "/index.html"), (200, b"multi".to_vec()));

    // TLS（http2 の既定 = [server] http2_enabled）: ALPN で h2
    let (alpn, _) = tls_alpn_get(ports.https, &[b"h2", b"http/1.1"], "/index.html").expect("h2");
    assert_eq!(alpn.as_deref(), Some(&b"h2"[..]), "{}", veil.log());
    let (_, resp) = tls_alpn_get(ports.https, &[b"http/1.1"], "/index.html").expect("TLS h1");
    let resp = resp.expect("HTTP/1.1 over TLS");
    assert_eq!(resp.status, 200, "{}", veil.log());
    #[cfg(feature = "http3")]
    {
        // Alt-Svc は HTTP/3 のリスナーの UDP ポート
        let want = format!("h3=\":{}\"", ports.https);
        assert!(
            resp.header("alt-svc").is_some_and(|v| v.contains(&want)),
            "alt-svc: {:?}",
            resp.headers
        );
        assert_eq!(
            h3_get_status(ports.https, "/index.html"),
            Ok(200),
            "{}",
            veil.log()
        );
    }

    // リダイレクトのリスナー: redirect_to のポートへ 301（HTTP/1.1・h2c とも）
    let want = format!("https://localhost:{}/index.html", ports.https);
    let resp = http1_get(ports.redirect, "/index.html").expect("301");
    assert_eq!(resp.status, 301, "{}", veil.log());
    assert_eq!(resp.header("location"), Some(want.as_str()));
    let (status, headers, _) = h2c_get_with_headers(ports.redirect, "/index.html");
    assert_eq!(status, 301);
    assert_eq!(
        headers
            .iter()
            .find(|(n, _)| n == "location")
            .map(|(_, v)| v.as_str()),
        Some(want.as_str())
    );

    // リロード: 平文の方針は反映され、リスナーの列の変更は警告して待ち受けは起動時のまま
    #[cfg(unix)]
    {
        std::fs::write(
            &cfg_path,
            f260_config(dir.path(), &ports, "allow_http = true"),
        )
        .unwrap();
        veil.sighup();
        assert!(
            wait_until(Duration::from_secs(10), || http1_get(
                ports.h2c,
                "/index.html"
            )
            .is_ok_and(|r| r.status == 200)),
            "allow_http = true must apply on reload: {}",
            veil.log()
        );
        let moved = free_port();
        let text = f260_config(dir.path(), &ports, "allow_http = true").replace(
            &format!("127.0.0.1:{}", ports.plain),
            &format!("127.0.0.1:{moved}"),
        );
        std::fs::write(&cfg_path, text).unwrap();
        veil.sighup();
        assert!(
            wait_until(Duration::from_secs(10), || veil
                .log()
                .contains("Listener settings changed (listener)")),
            "{}",
            veil.log()
        );
        // 古い待ち受けのまま（新しいポートは開かない）。方針の表も起動時の列のまま引かれる
        assert_eq!(
            http1_get(ports.plain, "/index.html").map(|r| r.status).ok(),
            Some(200)
        );
        assert!(http1_get(moved, "/index.html").is_err());
        assert_eq!(
            http1_get(ports.h2c, "/index.html").map(|r| r.status).ok(),
            Some(200)
        );
    }
}

/// F-260: `[[listener]]` の規則違反は `-t` で分かる。
#[test]
fn f260_listener_rules_are_validated() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    let run = |cfg: &str| {
        std::fs::write(&cfg_path, cfg).unwrap();
        let out = std::process::Command::new(veil_bin())
            .arg("-t")
            .arg("-c")
            .arg(&cfg_path)
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    // 同じポート（ワイルドカードと個別のアドレス）
    let (ok, err) = run(
        "[[listener]]\nlisten = \"0.0.0.0:18080\"\ntls = false\n[[listener]]\nlisten = \"127.0.0.1:18080\"\ntls = false\n",
    );
    assert!(!ok && err.contains("same address"), "{err}");
    // redirect_to の名前が無い
    let (ok, err) = run(
        "[[listener]]\nlisten = \"127.0.0.1:18080\"\ntls = false\nsecure_redirect = true\nredirect_to = \"nope\"\n",
    );
    assert!(!ok && err.contains("names no listener"), "{err}");
    // [server] を書かずに [[listener]] だけ（平文）は通る
    let (ok, err) = run("[[listener]]\nlisten = \"127.0.0.1:18080\"\ntls = false\n");
    assert!(ok, "{err}");
}

// ===================== F-273 =====================

/// h2c（`tls = false`）または TLS 上の h2 で、要求ヘッダ付きの GET を 1 つ送り (状態コード, 応答ヘッダ) を返す。
fn f273_h2_get(
    port: u16,
    tls: bool,
    path: &str,
    req: &[(&str, &str)],
) -> (u16, Vec<(String, String)>) {
    use std::sync::Arc;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        let scheme = if tls { "https" } else { "http" };
        let mut b = http::Request::builder()
            .method("GET")
            .uri(format!("{scheme}://localhost:{port}{path}"));
        for (k, v) in req {
            b = b.header(*k, *v);
        }
        let request = b.body(()).unwrap();
        async fn go<T>(io: T, request: http::Request<()>) -> (u16, Vec<(String, String)>)
        where
            T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
        {
            let (mut client, conn) = h2::client::handshake(io).await.expect("h2 handshake");
            tokio::spawn(async move {
                let _ = conn.await;
            });
            let (resp, _) = client.send_request(request, true).expect("send");
            let resp = resp.await.expect("response");
            let status = resp.status().as_u16();
            let headers = resp
                .headers()
                .iter()
                .map(|(n, v)| {
                    (
                        n.as_str().to_string(),
                        String::from_utf8_lossy(v.as_bytes()).into_owned(),
                    )
                })
                .collect();
            let mut body = resp.into_body();
            while let Some(chunk) = body.data().await {
                let chunk = chunk.expect("data");
                let _ = body.flow_control().release_capacity(chunk.len());
            }
            (status, headers)
        }
        if tls {
            let _ = veil::tls_provider::provider::default_provider().install_default();
            let mut cfg = rustls::ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerifyServer))
                .with_no_client_auth();
            cfg.alpn_protocols = vec![b"h2".to_vec()];
            let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
            let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
            let io = connector.connect(name, tcp).await.expect("tls");
            go(io, request).await
        } else {
            go(tcp, request).await
        }
    })
}

/// HTTP/3 で要求ヘッダ付きの GET を 1 つ送り (状態コード, 応答ヘッダ) を返す。
#[cfg(feature = "http3")]
fn f273_h3_get(port: u16, path: &str, req: &[(&str, &str)]) -> (u16, Vec<(String, String)>) {
    use bytes::Buf;
    use std::sync::Arc;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let _ = veil::tls_provider::provider::default_provider().install_default();
        let mut tls = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifyServer))
            .with_no_client_auth();
        tls.alpn_protocols = vec![b"h3".to_vec()];
        let qc = quinn::crypto::rustls::QuicClientConfig::try_from(Arc::new(tls)).unwrap();
        let mut ep = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        ep.set_default_client_config(quinn::ClientConfig::new(Arc::new(qc)));
        let conn = ep
            .connect(([127, 0, 0, 1], port).into(), "localhost")
            .unwrap()
            .await
            .unwrap();
        let (mut driver, mut send) = h3::client::new(h3_quinn::Connection::new(conn))
            .await
            .unwrap();
        let drive = tokio::spawn(async move {
            let _ = driver.wait_idle().await;
        });
        let mut b = http::Request::builder()
            .method("GET")
            .uri(format!("https://localhost:{port}{path}"));
        for (k, v) in req {
            b = b.header(*k, *v);
        }
        let mut stream = send.send_request(b.body(()).unwrap()).await.unwrap();
        stream.finish().await.unwrap();
        let resp = stream.recv_response().await.unwrap();
        while let Some(mut chunk) = stream.recv_data().await.unwrap() {
            let n = chunk.remaining();
            chunk.advance(n);
        }
        let headers = resp
            .headers()
            .iter()
            .map(|(n, v)| {
                (
                    n.as_str().to_string(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        drop(send);
        drive.abort();
        ep.close(0u32.into(), b"");
        (resp.status().as_u16(), headers)
    })
}

/// `[route.traffic.cors]` が実際の応答（プリフライト以外）にも効く（F-273）。Respond・File（sendfile / memory）・
/// Proxy（上流が CORS のヘッダを返す・返さない、`override_upstream`）・Redirect を、HTTP/1.1・h2c・TLS の HTTP/2・
/// HTTP/3 で確かめる。許可しないオリジンには付けず、`reject_disallowed` なら 403、CORS の無いルートは変わらない。
#[test]
fn f273_cors_actual_responses_all_protocols() {
    let dir = tempfile::tempdir().unwrap();
    let (cert, key) = write_self_signed(dir.path());
    let www = dir.path().join("www");
    std::fs::create_dir_all(&www).unwrap();
    std::fs::write(www.join("a.txt"), "static file").unwrap();
    let port = free_port();
    let tls_port = free_port();
    let cors = r#"[route.traffic.cors]
allow_origins = ["https://a.example"]
allow_methods = ["GET"]
expose_headers = ["X-Total"]
allow_credentials = true"#;
    let config = format!(
        r#"
[server]
listen = "127.0.0.1:{tls_port}"
listen_cleartext = "127.0.0.1:{port}"
enable_secure_redirect = false
allow_h2c = true
http2_enabled = true
threads = 1
[logging]
level = "warn"
[tls]
cert_path = "{cert}"
key_path = "{key}"
[http3]
enabled = true

# 上流役（CORS 無し）
[[route]]
[route.conditions]
path = "/plain"
[route.action]
type = "Respond"
body = "plain"

# 上流役（自分で Access-Control-Allow-Origin を返す）
[[route]]
[route.conditions]
path = "/up-acao/*"
[route.action]
type = "File"
path = "{www}"
mode = "sendfile"
[route.security]
add_response_headers = {{ "Access-Control-Allow-Origin" = "https://up.example" }}

[[route]]
[route.conditions]
path = "/respond"
[route.action]
type = "Respond"
body = "respond"
{cors}

[[route]]
[route.conditions]
path = "/static/*"
[route.action]
type = "File"
path = "{www}"
mode = "sendfile"
{cors}

[[route]]
[route.conditions]
path = "/mem"
[route.action]
type = "File"
path = "{www}/a.txt"
mode = "memory"
{cors}

[[route]]
[route.conditions]
path = "/proxy"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{port}/plain"
{cors}

[[route]]
[route.conditions]
path = "/proxy-keep/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{port}/up-acao"
{cors}

[[route]]
[route.conditions]
path = "/proxy-override/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{port}/up-acao"
{cors}
override_upstream = true

[[route]]
[route.conditions]
path = "/redir"
[route.action]
type = "Redirect"
redirect_url = "https://example.com/next"
redirect_status = 302
{cors}

[[route]]
[route.conditions]
path = "/strict"
[route.action]
type = "Respond"
body = "strict"
[route.traffic.cors]
allow_origins = ["https://a.example"]
reject_disallowed = true

[[route]]
[route.conditions]
path = "/any"
[route.action]
type = "Respond"
body = "any"
[route.traffic.cors]
allow_origins = ["*"]
"#,
        cert = tp(&cert),
        key = tp(&key),
        www = tp(&www),
    );
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, config).unwrap();
    let veil = VeilProcess::spawn(&cfg_path, &[], &[]);
    assert!(
        wait_for_port(port, Duration::from_secs(20))
            && wait_for_port(tls_port, Duration::from_secs(20)),
        "{}",
        veil.log()
    );

    type Get<'a> = Box<dyn Fn(&str, &[(&str, &str)]) -> (u16, Vec<(String, String)>) + 'a>;
    let mut protocols: Vec<(&str, Get)> = vec![
        (
            "HTTP/1.1",
            Box::new(|p: &str, h: &[(&str, &str)]| {
                let r = http1_request(port, "GET", p, h, b"").unwrap();
                (r.status, r.headers)
            }),
        ),
        (
            "h2c",
            Box::new(|p: &str, h: &[(&str, &str)]| f273_h2_get(port, false, p, h)),
        ),
        (
            "HTTP/2",
            Box::new(|p: &str, h: &[(&str, &str)]| f273_h2_get(tls_port, true, p, h)),
        ),
    ];
    #[cfg(feature = "http3")]
    protocols.push((
        "HTTP/3",
        Box::new(|p: &str, h: &[(&str, &str)]| f273_h3_get(tls_port, p, h)),
    ));
    let values = |hs: &[(String, String)], name: &str| -> Vec<String> {
        hs.iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
            .collect()
    };
    let ok_origin: &[(&str, &str)] = &[("Origin", "https://a.example")];
    let bad_origin: &[(&str, &str)] = &[("Origin", "https://evil.example")];
    for (proto, get) in &protocols {
        for (path, want_status, want_origin) in [
            ("/respond", 200, "https://a.example"),
            ("/static/a.txt", 200, "https://a.example"),
            ("/mem", 200, "https://a.example"),
            ("/proxy", 200, "https://a.example"),
            ("/proxy-keep/a.txt", 200, "https://up.example"),
            ("/proxy-override/a.txt", 200, "https://a.example"),
            ("/redir", 302, "https://a.example"),
        ] {
            let (st, hs) = get(path, ok_origin);
            let ctx = format!("{proto} {path}: {hs:?}\n{}", veil.log());
            assert_eq!(st, want_status, "{ctx}");
            assert_eq!(
                values(&hs, "access-control-allow-origin"),
                vec![want_origin.to_string()],
                "{ctx}"
            );
            if want_origin == "https://a.example" {
                assert_eq!(
                    values(&hs, "access-control-allow-credentials"),
                    vec!["true".to_string()],
                    "{ctx}"
                );
                assert_eq!(
                    values(&hs, "access-control-expose-headers"),
                    vec!["X-Total".to_string()],
                    "{ctx}"
                );
                assert!(values(&hs, "vary").iter().any(|v| v == "Origin"), "{ctx}");
            }
            // 許可しないオリジン: 処理はするが Access-Control-Allow-Origin は付けない（Vary だけ）
            let (st, hs) = get(path, bad_origin);
            let ctx = format!("{proto} {path} (evil): {hs:?}\n{}", veil.log());
            assert_eq!(st, want_status, "{ctx}");
            if !path.starts_with("/proxy-keep") {
                assert!(
                    values(&hs, "access-control-allow-origin").is_empty(),
                    "{ctx}"
                );
                assert!(values(&hs, "vary").iter().any(|v| v == "Origin"), "{ctx}");
            }
        }
        // reject_disallowed
        assert_eq!(get("/strict", bad_origin).0, 403, "{proto}");
        assert_eq!(get("/strict", ok_origin).0, 200, "{proto}");
        assert_eq!(get("/strict", &[]).0, 200, "{proto}");
        // "*": どのオリジンにも `*`、Vary は付けない
        let (_, hs) = get("/any", bad_origin);
        assert_eq!(
            values(&hs, "access-control-allow-origin"),
            vec!["*".to_string()],
            "{proto}"
        );
        assert!(values(&hs, "vary").is_empty(), "{proto} {hs:?}");
        // CORS の無いルートは変わらない
        let (st, hs) = get("/plain", ok_origin);
        assert_eq!(st, 200);
        assert!(
            hs.iter().all(
                |(n, _)| !n.to_ascii_lowercase().starts_with("access-control")
                    && !n.eq_ignore_ascii_case("vary")
            ),
            "{proto} {hs:?}"
        );
    }
}
