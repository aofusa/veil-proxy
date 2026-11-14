//! ACME クライアント（RFC 8555）: アカウント登録・注文・HTTP-01 認可・CSR 送信・証明書の取得。

use super::challenge;
use super::jws::{b64url, AccountKey};
use crate::ctrl_http::{HttpClient, HttpResponse};
use ftlog::info;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

const JOSE: &str = "application/jose+json";
const LIMIT: usize = 1024 * 1024;

pub struct AcmeClient<'a> {
    http: &'a HttpClient,
    key: &'a AccountKey,
    dir: Value,
    kid: Option<String>,
    nonce: Option<String>,
    /// ディレクトリを https で取ったか（サーバが返す URL も https に限る。B-397）
    https_only: bool,
}

impl<'a> AcmeClient<'a> {
    /// ディレクトリを取得する。
    pub fn new(
        http: &'a HttpClient,
        key: &'a AccountKey,
        directory_url: &str,
    ) -> Result<Self, String> {
        let r = http.request("GET", directory_url, &[], b"", LIMIT)?;
        if r.status != 200 {
            return Err(format!("ACME directory returned {}", r.status));
        }
        let dir: Value = serde_json::from_slice(&r.body)
            .map_err(|e| format!("invalid ACME directory: {}", e))?;
        Ok(AcmeClient {
            http,
            key,
            dir,
            kid: None,
            nonce: None,
            https_only: directory_url.starts_with("https://"),
        })
    }

    fn check_url(&self, url: &str) -> Result<(), String> {
        check_server_url(self.https_only, url)
    }

    fn dir_url(&self, name: &str) -> Result<String, String> {
        self.dir[name]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| format!("ACME directory has no '{}'", name))
    }

    fn fresh_nonce(&mut self) -> Result<String, String> {
        if let Some(n) = self.nonce.take() {
            return Ok(n);
        }
        let url = self.dir_url("newNonce")?;
        self.check_url(&url)?;
        let r = self.http.request("HEAD", &url, &[], b"", LIMIT)?;
        r.header("replay-nonce")
            .map(|s| s.to_string())
            .ok_or_else(|| "ACME server did not return a Replay-Nonce".to_string())
    }

    /// JWS で POST する（`payload` が `None` なら POST-as-GET）。badNonce は 1 回だけ再試行。
    fn post(&mut self, url: &str, payload: Option<&Value>) -> Result<HttpResponse, String> {
        self.check_url(url)?;
        for attempt in 0..2 {
            let nonce = self.fresh_nonce()?;
            let body = self.key.sign(url, &nonce, self.kid.as_deref(), payload)?;
            let r = self
                .http
                .request("POST", url, &[("Content-Type", JOSE)], &body, LIMIT)?;
            if let Some(n) = r.header("replay-nonce") {
                self.nonce = Some(n.to_string());
            }
            if r.status == 400 && attempt == 0 {
                let v: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
                if v["type"].as_str() == Some("urn:ietf:params:acme:error:badNonce") {
                    continue;
                }
            }
            if r.status >= 400 {
                return Err(format!(
                    "ACME {} returned {}: {}",
                    url,
                    r.status,
                    String::from_utf8_lossy(&r.body[..r.body.len().min(500)])
                ));
            }
            return Ok(r);
        }
        Err("ACME: repeated badNonce".into())
    }

    /// アカウントを作る（既にあれば同じアカウントが返る）。
    pub fn account(&mut self, contact: &[String], agree_tos: bool) -> Result<(), String> {
        let url = self.dir_url("newAccount")?;
        let payload = json!({ "termsOfServiceAgreed": agree_tos, "contact": contact });
        let r = self.post(&url, Some(&payload))?;
        let kid = r
            .header("location")
            .ok_or("ACME newAccount response has no Location")?
            .to_string();
        self.kid = Some(kid);
        Ok(())
    }

    /// `domains` の証明書を発行し、(証明書チェーン PEM, 秘密鍵 PEM) を返す。
    pub fn issue(
        &mut self,
        domains: &[String],
        timeout: Duration,
    ) -> Result<(String, String), String> {
        let deadline = Instant::now() + timeout;
        let url = self.dir_url("newOrder")?;
        let ids: Vec<Value> = domains
            .iter()
            .map(|d| json!({ "type": "dns", "value": d }))
            .collect();
        let r = self.post(&url, Some(&json!({ "identifiers": ids })))?;
        let order_url = r
            .header("location")
            .ok_or("ACME newOrder response has no Location")?
            .to_string();
        let order: Value = serde_json::from_slice(&r.body).map_err(|e| e.to_string())?;
        let thumb = self.key.thumbprint();
        let mut published = Vec::new();
        let result = (|| -> Result<(String, String), String> {
            for authz_url in order["authorizations"]
                .as_array()
                .cloned()
                .unwrap_or_default()
            {
                let authz_url = authz_url
                    .as_str()
                    .ok_or("bad authorization URL")?
                    .to_string();
                let authz: Value = serde_json::from_slice(&self.post(&authz_url, None)?.body)
                    .map_err(|e| e.to_string())?;
                if authz["status"] == "valid" {
                    continue;
                }
                let ch = authz["challenges"]
                    .as_array()
                    .and_then(|a| a.iter().find(|c| c["type"] == "http-01"))
                    .ok_or("no http-01 challenge offered")?
                    .clone();
                let token = ch["token"]
                    .as_str()
                    .ok_or("challenge without token")?
                    .to_string();
                let ch_url = ch["url"]
                    .as_str()
                    .ok_or("challenge without url")?
                    .to_string();
                challenge::publish(&token, &format!("{}.{}", token, thumb));
                published.push(token);
                self.post(&ch_url, Some(&json!({})))?;
                // 認可が valid になるまで待つ
                loop {
                    let a: Value = serde_json::from_slice(&self.post(&authz_url, None)?.body)
                        .map_err(|e| e.to_string())?;
                    match a["status"].as_str() {
                        Some("valid") => break,
                        Some("pending") | Some("processing") => {}
                        other => {
                            return Err(format!(
                                "authorization for {} became {:?}: {}",
                                a["identifier"]["value"], other, a["challenges"]
                            ))
                        }
                    }
                    wait(deadline)?;
                }
            }
            // 証明書の鍵と CSR
            let key = rcgen::KeyPair::generate().map_err(|e| e.to_string())?;
            let params =
                rcgen::CertificateParams::new(domains.to_vec()).map_err(|e| e.to_string())?;
            let csr = params.serialize_request(&key).map_err(|e| e.to_string())?;
            let finalize = order["finalize"]
                .as_str()
                .ok_or("order without finalize")?
                .to_string();
            self.post(&finalize, Some(&json!({ "csr": b64url(csr.der()) })))?;
            let cert_url = loop {
                let o: Value = serde_json::from_slice(&self.post(&order_url, None)?.body)
                    .map_err(|e| e.to_string())?;
                match o["status"].as_str() {
                    Some("valid") => {
                        break o["certificate"]
                            .as_str()
                            .ok_or("order without certificate")?
                            .to_string()
                    }
                    Some("pending") | Some("ready") | Some("processing") => {}
                    other => return Err(format!("order became {:?}: {}", other, o["error"])),
                }
                wait(deadline)?;
            };
            let cert = self.post(&cert_url, None)?;
            let chain = String::from_utf8(cert.body).map_err(|_| "certificate is not PEM text")?;
            if !chain.contains("BEGIN CERTIFICATE") {
                return Err("certificate response is not a PEM chain".into());
            }
            info!("[ACME] issued a certificate for {:?}", domains);
            Ok((chain, key.serialize_pem()))
        })();
        for t in published {
            challenge::withdraw(&t);
        }
        result
    }
}

fn wait(deadline: Instant) -> Result<(), String> {
    if Instant::now() >= deadline {
        return Err("ACME order timed out".into());
    }
    crate::server::cap_safe_sleep(Duration::from_millis(1000));
    Ok(())
}

/// サーバが返した URL を使ってよいか（B-397: ディレクトリを https で取ったら http へ落とさない。
/// 空白・制御文字は `ctrl_http::parse_url` が断る）
fn check_server_url(https_only: bool, url: &str) -> Result<(), String> {
    if https_only && !url.starts_with("https://") {
        return Err(format!(
            "ACME server returned a non-https URL ({})",
            url.escape_debug()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // B-397: https のディレクトリが返した http の URL は使わない
    #[test]
    fn server_urls_do_not_downgrade() {
        assert!(check_server_url(true, "https://acme.example/order/1").is_ok());
        assert!(check_server_url(true, "http://127.0.0.1:6379/x").is_err());
        assert!(check_server_url(false, "http://pebble:14000/order/1").is_ok());
    }
}
