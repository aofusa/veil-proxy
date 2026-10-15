//! proxy-injector が渡す環境変数（`LINKERD2_PROXY_*`）。対象の版（edge-26.10 / proxy v2.370）の注入テンプレートと
//! linkerd2-proxy の `linkerd/app/src/env.rs` から、veil が使うものだけを読む。

use std::net::SocketAddr;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub inbound_addr: SocketAddr,
    pub outbound_addr: SocketAddr,
    pub admin_addr: SocketAddr,
    /// 制御プレーン（`host:port`）と、その TLS のサーバの ID
    pub dst_addr: String,
    pub dst_name: String,
    pub policy_addr: String,
    pub policy_name: String,
    pub identity_addr: String,
    pub identity_name: String,
    /// 自分の ID（`sa.ns.serviceaccount.identity.linkerd.cluster.local`）
    pub local_name: String,
    /// `linkerd2-proxy-identity` が作った鍵（`key.p8`）と CSR（`csr.der`）の置き場所
    pub identity_dir: String,
    pub token_file: String,
    pub trust_anchors: String,
    /// `{"ns":"..","pod":".."}`（Policy の `workload`）
    pub workload: String,
    /// `{"ns":"..","nodeName":"..","pod":".."}`（Destination の `context_token`）
    pub dst_context: String,
    /// `all-unauthenticated` 等（Policy が届かないときの既定）
    pub default_policy: String,
    /// プロトコルの検出をしない inbound のポート（opaque）
    pub opaque_ports: Vec<u16>,
    pub cores: usize,
    pub shutdown_enabled: bool,
    pub outbound_connect_timeout: Duration,
    pub inbound_connect_timeout: Duration,
    pub log: String,
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn addr(name: &str, default: &str) -> SocketAddr {
    var(name)
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| default.parse().expect("default address"))
}

/// `100ms`・`3s`・`5m`・`1h` 形式。
pub fn duration(v: &str) -> Option<Duration> {
    let v = v.trim();
    let (n, unit) = v.split_at(v.find(|c: char| !c.is_ascii_digit())?);
    let n: u64 = n.parse().ok()?;
    Some(match unit {
        "ms" => Duration::from_millis(n),
        "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n * 60),
        "h" => Duration::from_secs(n * 3600),
        _ => return None,
    })
}

/// ポートの一覧（`25,587,3306` や範囲 `4000-4010`）。
pub fn ports(v: &str) -> Vec<u16> {
    let mut out = Vec::new();
    for part in v.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                if let (Ok(a), Ok(b)) = (a.parse::<u16>(), b.parse::<u16>()) {
                    out.extend(a..=b.min(a.saturating_add(4096)));
                }
            }
            None => {
                if let Ok(p) = part.parse() {
                    out.push(p);
                }
            }
        }
    }
    out
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let req = |n: &str| var(n).ok_or_else(|| format!("{} is not set", n));
        let d = |n: &str, def: Duration| var(n).and_then(|v| duration(&v)).unwrap_or(def);
        Ok(Config {
            inbound_addr: addr("LINKERD2_PROXY_INBOUND_LISTEN_ADDR", "0.0.0.0:4143"),
            outbound_addr: addr("LINKERD2_PROXY_OUTBOUND_LISTEN_ADDR", "127.0.0.1:4140"),
            admin_addr: addr("LINKERD2_PROXY_ADMIN_LISTEN_ADDR", "0.0.0.0:4191"),
            dst_addr: req("LINKERD2_PROXY_DESTINATION_SVC_ADDR")?,
            dst_name: var("LINKERD2_PROXY_DESTINATION_SVC_NAME").unwrap_or_default(),
            policy_addr: req("LINKERD2_PROXY_POLICY_SVC_ADDR")?,
            policy_name: var("LINKERD2_PROXY_POLICY_SVC_NAME").unwrap_or_default(),
            identity_addr: req("LINKERD2_PROXY_IDENTITY_SVC_ADDR")?,
            identity_name: var("LINKERD2_PROXY_IDENTITY_SVC_NAME").unwrap_or_default(),
            local_name: req("LINKERD2_PROXY_IDENTITY_LOCAL_NAME")?,
            identity_dir: var("LINKERD2_PROXY_IDENTITY_DIR")
                .unwrap_or_else(|| "/var/run/linkerd/identity/end-entity".into()),
            token_file: req("LINKERD2_PROXY_IDENTITY_TOKEN_FILE")?,
            trust_anchors: req("LINKERD2_PROXY_IDENTITY_TRUST_ANCHORS")?,
            workload: var("LINKERD2_PROXY_POLICY_WORKLOAD")
                .unwrap_or_default()
                .trim()
                .to_string(),
            dst_context: var("LINKERD2_PROXY_DESTINATION_CONTEXT")
                .unwrap_or_default()
                .trim()
                .to_string(),
            default_policy: var("LINKERD2_PROXY_INBOUND_DEFAULT_POLICY")
                .unwrap_or_else(|| "all-unauthenticated".into()),
            opaque_ports: var("LINKERD2_PROXY_INBOUND_PORTS_DISABLE_PROTOCOL_DETECTION")
                .map(|v| ports(&v))
                .unwrap_or_default(),
            cores: var("LINKERD2_PROXY_CORES")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1),
            shutdown_enabled: var("LINKERD2_PROXY_SHUTDOWN_ENDPOINT_ENABLED")
                .is_some_and(|v| v == "true"),
            outbound_connect_timeout: d(
                "LINKERD2_PROXY_OUTBOUND_CONNECT_TIMEOUT",
                Duration::from_secs(1),
            ),
            inbound_connect_timeout: d(
                "LINKERD2_PROXY_INBOUND_CONNECT_TIMEOUT",
                Duration::from_millis(100),
            ),
            log: var("LINKERD2_PROXY_LOG").unwrap_or_else(|| "warn".into()),
        })
    }

    /// `LINKERD2_PROXY_LOG`（`warn,linkerd=info,...`）から veil のログの水準を決める（`linkerd=` の指定を優先）。
    pub fn log_level(&self) -> &'static str {
        let mut lvl = "warn";
        for part in self.log.split(',') {
            let (k, v) = match part.split_once('=') {
                Some((k, v)) => (k.trim(), v.trim()),
                None => ("", part.trim()),
            };
            if k.is_empty() || k == "linkerd" {
                lvl = match v {
                    "trace" => "trace",
                    "debug" => "debug",
                    "info" => "info",
                    "error" => "error",
                    _ => "warn",
                };
            }
        }
        lvl
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations_and_ports() {
        assert_eq!(duration("100ms"), Some(Duration::from_millis(100)));
        assert_eq!(duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(duration("x"), None);
        assert_eq!(ports("25,587, 4000-4002"), vec![25, 587, 4000, 4001, 4002]);
    }
}
