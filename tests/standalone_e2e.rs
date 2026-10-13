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
    let guard = ChildGuard(child);
    assert!(
        wait_for_port(port, Duration::from_secs(15)),
        "control plane did not start"
    );
    guard
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

/// 子プロセスを Drop で止める。
struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
