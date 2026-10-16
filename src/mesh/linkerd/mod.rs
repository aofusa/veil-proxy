//! Linkerd のアダプタ（linkerd2-proxy の置き換え。F-223）。
//!
//! proxy-injector が注入するイメージ（`cr.l5d.io/linkerd/proxy`）の `/usr/lib/linkerd/linkerd2-proxy` を veil に
//! 差し替える。init（iptables）と `linkerd2-proxy-identity`（鍵と CSR を作って本体を exec する）は本家のまま。
//!
//! - 設定は環境変数（[`env`]）。制御プレーン（identity・destination・policy）は専用スレッドの gRPC（[`control`]）。
//! - データプレーンは Istio と同じメッシュのエンジン（HTTP/1.1・HTTP/2・L4・mTLS）を、Linkerd の取り決め
//!   （相手の ID は DNS の SAN、HTTP は ALPN なしの mTLS、HTTP/1.1 → HTTP/2 の昇格と `l5d-orig-proto`、
//!   opaque transport の transport header、`l5d-client-id`・`l5d-proxy-error`）で使う（[`conn`]）。
//! - 管理ポート 4191（[`admin`]）: `/ready`・`/live`・`/metrics`・`/shutdown`。

pub mod admin;
pub mod conn;
pub mod control;
pub mod discovery;
pub mod env;
pub mod identity;
pub mod metrics;
pub mod proto;
pub mod tap;

use crate::mesh::model::*;
use std::sync::Arc;

/// 自分の証明書の名前（メッシュの証明書の表）
pub const CERT: &str = "default";
/// 信頼アンカーの名前
pub const ROOTS: &str = "ROOTCA";

fn listener(name: &str, addr: std::net::SocketAddr, direction: Direction) -> Arc<MeshListener> {
    Arc::new(MeshListener {
        name: name.to_string(),
        addr,
        use_original_dst: false,
        bind_to_port: true,
        direction,
        chains: Vec::new(),
        default_chain: None,
        tls_inspector: false,
        http_inspector: false,
        needs_sniff: false,
        internal: false,
        matcher: None,
    })
}

/// Linkerd のプロキシとして動く（戻り値はプロセスの終了コード）。
pub fn main() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!(
            "linkerd2-proxy veil-{} (veil mesh data plane)",
            env!("CARGO_PKG_VERSION")
        );
        return 0;
    }
    let cfg = match env::Config::from_env() {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("veil (linkerd mode): {}", e);
            return 1;
        }
    };
    let lc = crate::logging::LoggingConfigSection {
        level: std::env::var("VEIL_MESH_LOG")
            .ok()
            .unwrap_or_else(|| cfg.log_level().to_string()),
        ..Default::default()
    };
    let _guard = crate::logging::init_logging(&lc);
    {
        // 既存のコンポーネント（TLS・HTTP/2）が読む実行時設定（kTLS は使わない）
        let mut rc = crate::config::RuntimeConfig::default();
        let mut k = (*rc.ktls_config).clone();
        k.enabled = false;
        k.enable_tx = false;
        k.enable_rx = false;
        k.fallback_enabled = true;
        rc.ktls_config = Arc::new(k);
        crate::config::CURRENT_CONFIG.store(Arc::new(rc));
    }
    if let Err(e) = crate::mesh::identity::set_roots_pem(ROOTS, cfg.trust_anchors.as_bytes()) {
        ftlog::error!("[linkerd] trust anchors: {}", e);
        return 1;
    }
    ftlog::info!(
        "[linkerd] veil {} as linkerd2-proxy for {} (inbound {}, outbound {}, admin {})",
        env!("CARGO_PKG_VERSION"),
        cfg.local_name,
        cfg.inbound_addr,
        cfg.outbound_addr,
        cfg.admin_addr
    );
    let _ = conn::CONFIG.set(cfg.clone());
    let mut listeners = vec![
        listener("inbound", cfg.inbound_addr, Direction::Inbound),
        listener("outbound", cfg.outbound_addr, Direction::Outbound),
        listener("admin", cfg.admin_addr, Direction::Inbound),
    ];
    if cfg.tap_name.is_some() {
        listeners.push(listener("control", cfg.control_addr, Direction::Inbound));
    }
    let _ = crate::mesh::server::STATIC_LISTENERS.set(listeners);
    crate::mesh::server::set_linkerd_mode();
    control::spawn(cfg.clone());
    crate::mesh::envoy::spawn_signal_thread(std::time::Duration::from_secs(5));
    crate::mesh::server::run(cfg.cores.max(1));
    ftlog::info!("[linkerd] stopped");
    0
}
