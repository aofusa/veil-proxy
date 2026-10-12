//! 自己完結型 E2E（veil バイナリをテストごとに一時設定で起動する）。
//!
//! `tests/e2e_setup.sh` が立てる共有環境（`e2e_tests.rs`）とは別に、プロセスの起動条件
//! （環境変数・設定ファイルの差し替え・シグナル）そのものを検証したいテストをここに置く。
//! バイナリは cargo が `CARGO_BIN_EXE_veil` で渡す（`cargo test --test standalone_e2e --features full`）。
//!
//! - F-179: 環境変数 `VEIL_OVERRIDE_<n>` による設定上書き（SIGHUP 後も保たれる）
//! - F-180: 証明書の Kubernetes Secret 形式（`..data` シンボリックリンク差し替え）への追従
//! - F-188: TLS 無しの平文専用サーバ（`[server].h2c_http1`）
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
    let (cert, key) = write_self_signed(dir.path());
    let www = dir.path().join("www");
    std::fs::create_dir_all(&www).unwrap();
    std::fs::write(www.join("index.html"), "v1").unwrap();

    let file_port = free_port();
    let env_port = free_port();
    let h2c_port = free_port();
    let config = format!(
        r#"
[server]
listen = "127.0.0.1:{file_port}"
threads = 1
tls_only = false
h2c_enabled = true
h2c_listen = "127.0.0.1:{h2c_port}"
[tls]
cert_path = "{cert}"
key_path = "{key}"
[logging]
level = "warn"
[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "{www}"
"#,
        cert = cert.display(),
        key = key.display(),
        www = www.display(),
    );
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, &config).unwrap();

    let veil = VeilProcess::spawn(
        &cfg_path,
        &[(
            "VEIL_OVERRIDE_1",
            format!(r#"server.listen = "127.0.0.1:{env_port}""#).as_str(),
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
    let www2 = dir.path().join("www2");
    std::fs::create_dir_all(&www2).unwrap();
    std::fs::write(www2.join("index.html"), "v2").unwrap();
    std::fs::write(
        &cfg_path,
        config.replace(&www.display().to_string(), &www2.display().to_string()),
    )
    .unwrap();
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
    let (cert, key) = write_self_signed(dir.path());
    for (name, body) in [("a", "A"), ("b", "B"), ("c", "C")] {
        let d = dir.path().join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("index.html"), body).unwrap();
    }
    let port = free_port();
    let h2c_port = free_port();
    let config = format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
threads = 1
tls_only = false
h2c_enabled = true
h2c_listen = "127.0.0.1:{h2c_port}"
[tls]
cert_path = "{cert}"
key_path = "{key}"
[logging]
level = "warn"
[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "{a}"
"#,
        cert = cert.display(),
        key = key.display(),
        a = dir.path().join("a").display(),
    );
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, &config).unwrap();

    let b = dir.path().join("b").display().to_string();
    let c = dir.path().join("c").display().to_string();
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
        secret = secret.display(),
        www = www.display(),
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
