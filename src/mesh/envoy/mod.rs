//! Envoy 互換の起動（F-222）: Istio の pilot-agent から `/usr/local/bin/envoy` として起動される。
//!
//! pilot-agent が渡すコマンドライン（`-c <bootstrap>`・`--concurrency` 等）とブートストラップを読み、
//! xDS（ADS）・SDS の購読、管理 API、ワーカーを起動する。pilot-agent・istiod は無改造で使う。

pub mod bootstrap;
pub mod sds;

use std::sync::atomic::Ordering;
use std::time::Duration;

/// pilot-agent が Envoy に渡すコマンドラインのうち使うもの。
#[derive(Debug, Default)]
pub struct Cli {
    pub config_path: Option<String>,
    pub concurrency: Option<usize>,
    pub log_level: Option<String>,
    pub drain_time: Option<Duration>,
    pub version: bool,
}

pub fn parse_cli(args: &[String]) -> Cli {
    let mut c = Cli::default();
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let (key, inline) = match a.split_once('=') {
            Some((k, v)) if k.starts_with("--") => (k, Some(v.to_string())),
            _ => (a, None),
        };
        let mut val = || -> Option<String> {
            if let Some(v) = &inline {
                return Some(v.clone());
            }
            i += 1;
            args.get(i).cloned()
        };
        match key {
            "-c" | "--config-path" => c.config_path = val(),
            "--concurrency" => c.concurrency = val().and_then(|v| v.parse().ok()),
            "-l" | "--log-level" => c.log_level = val(),
            "--drain-time-s" => {
                c.drain_time = val().and_then(|v| v.parse().ok()).map(Duration::from_secs)
            }
            "--version" => c.version = true,
            // 値を取るが使わないフラグ（pilot-agent が渡す）
            "--drain-strategy"
            | "--parent-shutdown-time-s"
            | "--local-address-ip-version"
            | "--file-flush-interval-msec"
            | "--component-log-level"
            | "--service-cluster"
            | "--service-node"
            | "--config-yaml"
            | "--log-format"
            | "--restart-epoch"
            | "--base-id" => {
                let _ = val();
            }
            _ => {}
        }
        i += 1;
    }
    c
}

fn log_level(l: Option<&str>) -> &'static str {
    match l.unwrap_or("info") {
        "trace" => "trace",
        "debug" => "debug",
        "warning" | "warn" => "warn",
        "error" | "critical" => "error",
        "off" => "error",
        _ => "info",
    }
}

/// Envoy として動く（戻り値はプロセスの終了コード）。
pub fn main(args: Vec<String>) -> i32 {
    let cli = parse_cli(&args);
    if cli.version {
        println!(
            "envoy  veil-{}/Clean/RELEASE/BoringSSL (veil mesh data plane)",
            env!("CARGO_PKG_VERSION")
        );
        return 0;
    }
    let Some(path) = cli.config_path.clone() else {
        eprintln!("veil (envoy mode): -c <bootstrap> is required");
        return 1;
    };
    // 理由付き allow: 起動時に 1 回だけ読む（データプレーン開始前）
    #[allow(clippy::disallowed_methods)]
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("veil (envoy mode): cannot read {}: {}", path, e);
            return 1;
        }
    };
    // ログ（pilot-agent は -l warning を渡す。VEIL_MESH_LOG で上書きできる）
    let mut lc = crate::logging::LoggingConfigSection::default();
    let lvl = std::env::var("VEIL_MESH_LOG").ok();
    lc.level = log_level(lvl.as_deref().or(cli.log_level.as_deref())).to_string();
    let _guard = crate::logging::init_logging(&lc);

    // 既存のコンポーネント（TLS・HTTP/2）が読む実行時設定。kTLS は VEIL_MESH_KTLS=1 のときだけ
    // （ノードに tls モジュールが無いと接続ごとにフォールバックの警告が出るため既定は無効）
    {
        let mut rc = crate::config::RuntimeConfig::default();
        let ktls = std::env::var("VEIL_MESH_KTLS")
            .map(|v| v == "1")
            .unwrap_or(false);
        let mut k = (*rc.ktls_config).clone();
        k.enabled = ktls;
        k.enable_tx = ktls;
        k.enable_rx = ktls;
        k.fallback_enabled = true;
        rc.ktls_config = std::sync::Arc::new(k);
        crate::config::CURRENT_CONFIG.store(std::sync::Arc::new(rc));
    }

    let b = match bootstrap::parse(&text) {
        Ok(b) => b,
        Err(e) => {
            ftlog::error!("[envoy] bootstrap {}: {}", path, e);
            return 1;
        }
    };
    if let Some(md) = &b.node.metadata {
        crate::mesh::peer::init_local(&b.node.id, md);
    }
    ftlog::info!(
        "[envoy] veil {} as Envoy for node {} (xDS: {:?}, SDS: {:?})",
        env!("CARGO_PKG_VERSION"),
        b.node.id,
        b.ads_uds,
        b.sds_uds
    );
    let _ = crate::mesh::server::STATIC_LISTENERS.set(b.static_listeners);
    if let Some(sds) = b.sds_uds.clone() {
        sds::spawn(sds, b.node.clone());
    }
    match b.ads_uds.clone() {
        Some(ads) => crate::mesh::xds::ads::spawn(crate::mesh::xds::ads::AdsConfig {
            uds_path: ads,
            node: b.node.clone(),
            workload_discovery: b.workload_discovery,
        }),
        None => ftlog::warn!("[envoy] no ADS cluster in the bootstrap"),
    }
    if let Some(admin) = b.admin {
        crate::mesh::admin::spawn(admin);
    }
    spawn_signal_thread(cli.drain_time.unwrap_or(Duration::from_secs(5)));
    let workers = cli
        .concurrency
        .filter(|n| *n > 0)
        .unwrap_or_else(num_cpus::get);
    // ambient の HBONE（HTTP/2 CONNECT）を受ける
    crate::http2::connection::ALLOW_CONNECT.store(true, std::sync::atomic::Ordering::Relaxed);
    crate::mesh::server::run(workers);
    ftlog::info!("[envoy] stopped");
    0
}

/// SIGTERM・SIGINT で排出してから止める（pilot-agent は先に `/drain_listeners` を呼ぶ）。
pub(crate) fn spawn_signal_thread(drain: Duration) {
    use signal_hook::consts::{SIGINT, SIGTERM};
    let mut signals = match signal_hook::iterator::Signals::new([SIGINT, SIGTERM]) {
        Ok(s) => s,
        Err(e) => {
            ftlog::warn!("[envoy] cannot install signal handlers: {}", e);
            return;
        }
    };
    let _ = std::thread::Builder::new()
        .name("veil-signal".into())
        .spawn(move || {
            if signals.forever().next().is_some() {
                ftlog::info!("[envoy] shutting down (drain {:?})", drain);
                crate::mesh::state::state()
                    .draining
                    .store(true, Ordering::Relaxed);
                // 理由付き allow: シグナル専用スレッド（データプレーン外）
                #[allow(clippy::disallowed_methods)]
                std::thread::sleep(drain.min(Duration::from_secs(5)));
                crate::config::SHUTDOWN_FLAG.store(true, Ordering::Relaxed);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pilot_agent_command_line() {
        let args: Vec<String> = "-c etc/istio/proxy/envoy-rev.json --drain-time-s 45 --drain-strategy immediate --local-address-ip-version v4 --file-flush-interval-msec 1000 --disable-hot-restart --allow-unknown-static-fields -l warning --component-log-level misc:error --skip-deprecated-logs --concurrency 2"
            .split(' ')
            .map(|s| s.to_string())
            .collect();
        let c = parse_cli(&args);
        assert_eq!(
            c.config_path.as_deref(),
            Some("etc/istio/proxy/envoy-rev.json")
        );
        assert_eq!(c.concurrency, Some(2));
        assert_eq!(c.log_level.as_deref(), Some("warning"));
        assert_eq!(c.drain_time, Some(Duration::from_secs(45)));
        assert_eq!(log_level(Some("warning")), "warn");
    }
}
