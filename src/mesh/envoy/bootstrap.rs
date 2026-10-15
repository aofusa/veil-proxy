//! Envoy のブートストラップ（pilot-agent が生成する `envoy-rev.json`）を読む（F-222）。
//!
//! 使うのは node（id・cluster・metadata・locality）、管理 API のアドレス、ADS と SDS の UDS
//! （`xds-grpc`・`sds-grpc` クラスタの `pipe`）、静的なリスナー（15021 の健康・15090 の Prometheus）と
//! 静的なクラスタ（`agent`・`prometheus_stats`）。静的なリスナーはメッシュモデルへ直接変換する。

use crate::mesh::model::*;
use crate::mesh::xds::proto as p;
use bytes::Bytes;
use serde_json::Value;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub struct Bootstrap {
    pub node: p::Node,
    pub admin: Option<SocketAddr>,
    pub ads_uds: Option<PathBuf>,
    pub sds_uds: Option<PathBuf>,
    pub static_listeners: Vec<Arc<MeshListener>>,
}

enum StaticAddr {
    Socket(SocketAddr),
    Pipe(PathBuf),
}

fn addr_of(v: &Value) -> Option<StaticAddr> {
    if let Some(sa) = v.get("socket_address") {
        let ip: std::net::IpAddr = sa.get("address")?.as_str()?.parse().ok()?;
        let port = sa.get("port_value")?.as_u64()? as u16;
        return Some(StaticAddr::Socket(SocketAddr::new(ip, port)));
    }
    if let Some(pp) = v.get("pipe") {
        return Some(StaticAddr::Pipe(resolve_path(pp.get("path")?.as_str()?)));
    }
    None
}

/// pilot-agent は相対パス（`./etc/istio/proxy/XDS`）を渡す。Envoy は cwd から解決する。
fn resolve_path(p: &str) -> PathBuf {
    let pb = PathBuf::from(p);
    if pb.is_absolute() {
        return pb;
    }
    std::env::current_dir().map(|d| d.join(&pb)).unwrap_or(pb)
}

fn cluster_endpoint(c: &Value) -> Option<StaticAddr> {
    let eps = c.get("load_assignment")?.get("endpoints")?.as_array()?;
    for e in eps {
        for lb in e.get("lb_endpoints")?.as_array()? {
            if let Some(a) = lb
                .get("endpoint")
                .and_then(|ep| ep.get("address"))
                .and_then(addr_of)
            {
                return Some(a);
            }
        }
    }
    None
}

pub fn parse(text: &str) -> Result<Bootstrap, String> {
    let v: Value = if text.trim_start().starts_with('{') {
        serde_json::from_str(text).map_err(|e| format!("bootstrap JSON: {e}"))?
    } else {
        return Err("only JSON bootstraps are supported".into());
    };
    let n = v.get("node").cloned().unwrap_or(Value::Null);
    let node = p::Node {
        id: n
            .get("id")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        cluster: n
            .get("cluster")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        metadata: n.get("metadata").map(crate::mesh::xds::json_to_struct),
        locality: n.get("locality").map(|l| p::Locality {
            region: l
                .get("region")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .into(),
            zone: l.get("zone").and_then(|x| x.as_str()).unwrap_or("").into(),
            sub_zone: l
                .get("sub_zone")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .into(),
        }),
        user_agent_name: "envoy".into(),
        user_agent_version: concat!("veil/", env!("CARGO_PKG_VERSION")).into(),
        client_features: vec![
            "envoy.lb.does_not_support_overprovisioning".into(),
            "envoy.lrs.supports_send_all_clusters".into(),
        ],
    };
    let admin = v
        .get("admin")
        .and_then(|a| a.get("address"))
        .and_then(addr_of)
        .and_then(|a| match a {
            StaticAddr::Socket(s) => Some(s),
            _ => None,
        });

    let statics = v.get("static_resources").cloned().unwrap_or(Value::Null);
    let mut clusters: HashMap<String, (Value, Option<StaticAddr>)> = HashMap::new();
    if let Some(cs) = statics.get("clusters").and_then(|c| c.as_array()) {
        for c in cs {
            let name = c
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let ep = cluster_endpoint(c);
            clusters.insert(name, (c.clone(), ep));
        }
    }
    let grpc_cluster = |cfg: Option<&Value>| -> Option<String> {
        cfg?.get("grpc_services")?
            .as_array()?
            .first()?
            .get("envoy_grpc")?
            .get("cluster_name")?
            .as_str()
            .map(|s| s.to_string())
    };
    let ads_cluster = grpc_cluster(v.get("dynamic_resources").and_then(|d| d.get("ads_config")))
        .unwrap_or_else(|| "xds-grpc".into());
    let pipe_of = |name: &str| match clusters.get(name).and_then(|c| c.1.as_ref()) {
        Some(StaticAddr::Pipe(p)) => Some(p.clone()),
        _ => None,
    };
    let ads_uds = pipe_of(&ads_cluster);
    let sds_uds = pipe_of("sds-grpc");

    // 静的なクラスタ（ソケットのもの）をスロットに登録する
    {
        let mut reg = crate::mesh::state::state()
            .registry
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (name, (_, ep)) in &clusters {
            let Some(StaticAddr::Socket(addr)) = ep else {
                continue;
            };
            let slot = reg.cluster_slot(name);
            slot.cfg
                .store(Arc::new(Some(Arc::new(static_cluster(name)))));
            slot.endpoints
                .store(Arc::new(EndpointSet::new(vec![MeshEndpoint::new(*addr)])));
        }
    }

    // 静的なリスナー（HCM のインラインのルートで静的クラスタへ）
    let mut static_listeners = Vec::new();
    if let Some(ls) = statics.get("listeners").and_then(|l| l.as_array()) {
        for l in ls {
            match static_listener(l) {
                Ok(Some(ml)) => static_listeners.push(Arc::new(ml)),
                Ok(None) => {}
                Err(e) => ftlog::warn!("[envoy] static listener: {}", e),
            }
        }
    }
    Ok(Bootstrap {
        node,
        admin,
        ads_uds,
        sds_uds,
        static_listeners,
    })
}

fn static_cluster(name: &str) -> ClusterCfg {
    ClusterCfg {
        name: name.to_string(),
        kind: ClusterKind::Static,
        lb: LbPolicy::RoundRobin,
        connect_timeout: Duration::from_secs(1),
        bind: None,
        tls_matches: Vec::new(),
        tls_default: None,
        proto: UpstreamProto::Http1,
        idle_timeout: Duration::from_secs(60),
        max_requests_per_conn: 0,
        outlier: None,
        max_connections: 1024,
        max_pending: 1024,
        max_requests: 1024,
        max_retries: 0,
        service_host: Bytes::new(),
        service_name: Bytes::new(),
        service_namespace: Bytes::new(),
        eds_name: name.to_string(),
        port_override: None,
        svc_labels: None,
    }
}

fn static_listener(l: &Value) -> Result<Option<MeshListener>, String> {
    let name = l
        .get("name")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let Some(StaticAddr::Socket(addr)) = l.get("address").and_then(addr_of) else {
        return Ok(None);
    };
    let fc = l
        .get("filter_chains")
        .and_then(|f| f.as_array())
        .and_then(|a| a.first())
        .ok_or("no filter chain")?;
    let hcm = fc
        .get("filters")
        .and_then(|f| f.as_array())
        .and_then(|a| {
            a.iter().find(|f| {
                f.get("name").and_then(|n| n.as_str())
                    == Some("envoy.filters.network.http_connection_manager")
            })
        })
        .and_then(|f| f.get("typed_config"))
        .ok_or("no HCM")?;
    let rc = hcm.get("route_config").ok_or("no inline route_config")?;
    let mut table = RouteTable::empty(&name);
    let mut reg = crate::mesh::state::state()
        .registry
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (vi, vh) in rc
        .get("virtual_hosts")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .enumerate()
    {
        let mut routes = Vec::new();
        for r in vh
            .get("routes")
            .and_then(|r| r.as_array())
            .into_iter()
            .flatten()
        {
            let prefix = r
                .get("match")
                .and_then(|m| m.get("prefix"))
                .and_then(|p| p.as_str())
                .unwrap_or("/")
                .to_string();
            let Some(cluster) = r
                .get("route")
                .and_then(|a| a.get("cluster"))
                .and_then(|c| c.as_str())
            else {
                continue;
            };
            routes.push(MeshRoute {
                name: String::new(),
                path: PathMatch::Prefix(prefix),
                case_sensitive: true,
                headers: Vec::new(),
                query: Vec::new(),
                fraction: None,
                grpc_only: false,
                action: RouteAction::Forward(Arc::new(ForwardAction {
                    cluster_not_found: 503,
                    target: ClusterTarget::Single(reg.cluster_slot(cluster)),
                    timeout: None,
                    idle_timeout: None,
                    retry: None,
                    prefix_rewrite: None,
                    regex_rewrite: None,
                    host_rewrite: None,
                    auto_host_rewrite: false,
                    mirrors: Vec::new(),
                    hash: Vec::new(),
                    websocket: None,
                    connect_terminate: false,
                })),
                request_ops: HeaderOps::default(),
                response_ops: HeaderOps::default(),
                operation: Bytes::new(),
                fault: None,
                cors: None,
                rbac: None,
            });
        }
        table.vhosts.push(VirtualHost {
            name: vh.get("name").and_then(|n| n.as_str()).unwrap_or("").into(),
            routes,
            request_ops: HeaderOps::default(),
            response_ops: HeaderOps::default(),
            include_attempt_count: false,
            retry: None,
            cors: None,
            fault: None,
        });
        table.catch_all.get_or_insert(vi);
    }
    drop(reg);
    let hcm = HttpCm {
        stat_prefix: name.clone(),
        routes: RouteRef::Inline(Arc::new(table)),
        codec: Codec::Auto,
        server_name: Bytes::from_static(b"envoy"),
        xfcc: XfccMode::Sanitize,
        xfcc_subject: false,
        xfcc_uri: false,
        xfcc_dns: false,
        generate_request_id: false,
        use_remote_address: false,
        websocket: false,
        filters: Vec::new(),
        peer_metadata: false,
        istio_alpn: false,
        direction: Direction::Unspecified,
        stream_idle_timeout: None,
        trace_sampling: 0,
        access_logs: Vec::new(),
        tracer: None,
        linkerd: false,
    };
    Ok(Some(MeshListener {
        name,
        addr,
        use_original_dst: false,
        bind_to_port: true,
        direction: Direction::Unspecified,
        chains: vec![Arc::new(FilterChain {
            name: String::new(),
            m: ChainMatch::default(),
            tls: None,
            action: ChainAction::Http(Arc::new(hcm)),
            stats: false,
        })],
        default_chain: None,
        tls_inspector: false,
        http_inspector: true,
        needs_sniff: true,
        internal: false,
        matcher: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_istio_bootstrap() {
        // Istio 1.31.1 の pilot-agent が生成した実物
        let b = parse(include_str!("testdata/istio_bootstrap.json")).unwrap();
        assert!(b.node.id.starts_with("sidecar~"));
        assert_eq!(b.admin, Some("127.0.0.1:15000".parse().unwrap()));
        assert!(b.ads_uds.unwrap().ends_with("etc/istio/proxy/XDS"));
        assert!(b.sds_uds.unwrap().ends_with("workload-spiffe-uds/socket"));
        let ports: Vec<u16> = b.static_listeners.iter().map(|l| l.addr.port()).collect();
        assert!(
            ports.contains(&15021) && ports.contains(&15090),
            "{ports:?}"
        );
        let md = b.node.metadata.unwrap();
        assert_eq!(md.get_str("NAMESPACE"), Some("demo"));
    }
}
