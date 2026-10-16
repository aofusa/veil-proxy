//! Linkerd の identity（F-223）: `linkerd2-proxy-identity` が作った鍵（`key.p8`）と CSR（`csr.der`）、
//! サービスアカウントのトークンを `io.linkerd.proxy.identity.Identity/Certify` に送って証明書を受け取る。
//! 有効期間の 70% で更新する（本家と同じ。最短 10 秒・最長 1 日）。証明書が届くまで `/ready` は 503。

use super::control::{self, CtlTarget};
use super::env::Config;
use super::proto as p;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const PATH: &str = "/io.linkerd.proxy.identity.Identity/Certify";

/// 証明書が届いた（`/ready`）
pub static READY: AtomicBool = AtomicBool::new(false);
/// 証明書の有効期限（UNIX 秒。管理 API の表示用）
pub static EXPIRY: AtomicI64 = AtomicI64::new(0);

// 理由付き allow: 制御プレーン用の専用スレッドで小さなファイルを読む（データプレーンではない）
#[allow(clippy::disallowed_methods)]
fn read(path: &str) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("{}: {}", path, e))
}

pub async fn run(cfg: Arc<Config>) {
    let t = CtlTarget {
        addr: cfg.identity_addr.clone(),
        name: cfg.identity_name.clone(),
        client_cert: false,
    };
    let mut backoff = Duration::from_millis(200);
    loop {
        if crate::config::SHUTDOWN_FLAG.load(Ordering::Relaxed) {
            return;
        }
        match certify(&cfg, &t).await {
            Ok(expiry) => {
                backoff = Duration::from_millis(200);
                READY.store(true, Ordering::Relaxed);
                let left = expiry.duration_since(SystemTime::now()).unwrap_or_default();
                let wait =
                    (left * 7 / 10).clamp(Duration::from_secs(10), Duration::from_secs(86_400));
                ftlog::info!(
                    "[linkerd] certified {} (expires in {}s, refresh in {}s)",
                    cfg.local_name,
                    left.as_secs(),
                    wait.as_secs()
                );
                crate::runtime::time::sleep(wait).await;
            }
            Err(e) => {
                ftlog::warn!("[linkerd] identity: {}", e);
                crate::runtime::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(10));
            }
        }
    }
}

async fn certify(cfg: &Config, t: &CtlTarget) -> Result<SystemTime, String> {
    let token = read(&cfg.token_file)?;
    let csr = read(&format!("{}/csr.der", cfg.identity_dir))?;
    let key = read(&format!("{}/key.p8", cfg.identity_dir))?;
    let req = p::CertifyRequest {
        identity: cfg.local_name.clone(),
        token,
        certificate_signing_request: csr,
    };
    let resp: p::CertifyResponse = control::unary(t, PATH, &req, Duration::from_secs(10)).await?;
    let exp = resp
        .valid_until
        .ok_or("certify response without valid_until")?;
    let expiry = UNIX_EPOCH + Duration::from_secs(exp.seconds.max(0) as u64);
    if expiry <= SystemTime::now() {
        return Err("certificate already expired".into());
    }
    let mut chain: Vec<CertificateDer<'static>> = vec![CertificateDer::from(resp.leaf_certificate)];
    chain.extend(
        resp.intermediate_certificates
            .into_iter()
            .map(CertificateDer::from),
    );
    crate::mesh::identity::set_cert_der(
        super::CERT,
        chain,
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
    )?;
    EXPIRY.store(exp.seconds, Ordering::Relaxed);
    Ok(expiry)
}
