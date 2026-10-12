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

// ===================== F-188（本体側の小改修） =====================

/// `[tls]` を省略した平文専用サーバ（H2C 専用構成 + `h2c_http1 = true`）で、同じポートに
/// 平文 HTTP/1.1 と h2c（prior knowledge）の両方が来ても処理できる。
#[test]
fn f188_plaintext_only_server_without_tls_serves_http1_and_h2c() {
    let dir = tempfile::tempdir().unwrap();
    let www = dir.path().join("www");
    std::fs::create_dir_all(&www).unwrap();
    std::fs::write(www.join("index.html"), "plain").unwrap();
    let port = free_port();
    let config = format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
threads = 1
h2c_enabled = true
h2c_http1 = true
[logging]
level = "warn"
[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "{www}"
"#,
        www = www.display()
    );
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, config).unwrap();
    let veil = VeilProcess::spawn(&cfg_path, &[], &[]);
    assert!(
        wait_for_port(port, Duration::from_secs(20)),
        "{}",
        veil.log()
    );

    let resp = http1_get(port, "/index.html").expect("plain HTTP/1.1");
    assert_eq!(resp.status, 200, "{}", veil.log());
    assert_eq!(resp.text(), "plain");

    let (status, body) = h2c_get(port, "/index.html");
    assert_eq!(status, 200);
    assert_eq!(body, b"plain");
}

/// `h2c_http1` を付けない H2C 専用構成は従来どおり HTTP/1.1 を受けない（既定の挙動は不変）。
#[test]
fn f188_h2c_only_without_h2c_http1_still_rejects_http1() {
    let dir = tempfile::tempdir().unwrap();
    let www = dir.path().join("www");
    std::fs::create_dir_all(&www).unwrap();
    std::fs::write(www.join("index.html"), "x").unwrap();
    let port = free_port();
    let config = format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
threads = 1
h2c_enabled = true
[logging]
level = "warn"
[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "{www}"
"#,
        www = www.display()
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
        http1_get(port, "/index.html").is_err(),
        "HTTP/1.1 must be rejected without h2c_http1"
    );
    let (status, _) = h2c_get(port, "/index.html");
    assert_eq!(status, 200);
}

/// `[tls]` を省略できるのは平文専用サーバ構成だけ（TLS リスナーを使う構成は -t で失敗）。
#[test]
fn f188_missing_tls_is_rejected_unless_plaintext_only() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, "[server]\nlisten = \"127.0.0.1:18443\"\n").unwrap();
    let out = std::process::Command::new(veil_bin())
        .arg("-t")
        .arg("-c")
        .arg(&cfg_path)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("TLS certificate is required"), "{}", stderr);

    std::fs::write(
        &cfg_path,
        "[server]\nlisten = \"127.0.0.1:18443\"\nh2c_enabled = true\nh2c_http1 = true\n",
    )
    .unwrap();
    let out = std::process::Command::new(veil_bin())
        .arg("-t")
        .arg("-c")
        .arg(&cfg_path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ===================== F-04 VDS =====================

/// 参照制御プレーン（tools/vds/vds_server.py）からルートを配信し、反映・NACK・トークン認証を確かめる。
#[cfg(feature = "vds")]
#[test]
fn f04_vds_delivers_routes_and_keeps_previous_on_nack() {
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
            p.display()
        )
    };
    std::fs::write(&snap, route(&dir.path().join("a"))).unwrap();
    let token = dir.path().join("token");
    std::fs::write(&token, "s3cret\n").unwrap();

    let cp_port = free_port();
    let cp = ChildGuard(
        std::process::Command::new("python3")
            .arg("-I")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tools/vds/vds_server.py"
            ))
            .arg("--snapshot")
            .arg(&snap)
            .arg("--listen")
            .arg(format!("127.0.0.1:{}", cp_port))
            .arg("--token-file")
            .arg(&token)
            .arg("--hold")
            .arg("5")
            .arg("--watch-interval")
            .arg("0.2")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("python3 is required for the VDS E2E"),
    );
    assert!(wait_for_port(cp_port, Duration::from_secs(10)));

    let port = free_port();
    let config = format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
threads = 1
h2c_enabled = true
h2c_http1 = true
[logging]
level = "info"
[vds]
enabled = true
server = "http://127.0.0.1:{cp_port}"
node_id = "e2e"
token_file = "{token}"
poll_timeout_secs = 5
retry_initial_ms = 100
{base_route}"#,
        token = token.display(),
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
    assert!(
        wait_until(Duration::from_secs(15), body_is("A")),
        "VDS snapshot A not applied; log:\n{}",
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
        http1_get(cp_port, "/v1/status")
            .map(|r| r.text().contains("\"nacks\": 1") || r.text().contains("\"nacks\":1"))
            .unwrap_or(false)
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
