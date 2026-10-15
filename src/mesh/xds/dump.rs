//! 管理 API の `config_dump`・`clusters`・`listeners`（istioctl `proxy-config` が読む。F-222）。
//!
//! 受け取った xDS のリソースのうち主要なフィールドを Envoy の JSON（protojson）の形で出す。
//! 鍵は出さない。

use super::ads::RAW;
use super::proto as p;
use serde_json::{Value, json};

fn addr_json(a: &Option<p::Address>) -> Value {
    match a.as_ref().and_then(|a| a.address.as_ref()) {
        Some(p::address::Address::SocketAddress(sa)) => json!({
            "socket_address": {"address": sa.address, "port_value": sa.port_value}
        }),
        Some(p::address::Address::Pipe(pp)) => json!({"pipe": {"path": pp.path}}),
        Some(p::address::Address::EnvoyInternalAddress(ia)) => json!({
            "envoy_internal_address": {
                "server_listener_name": ia.server_listener_name,
                "endpoint_id": ia.endpoint_id
            }
        }),
        None => Value::Null,
    }
}

/// フィルタの typed_config（istioctl は種類と HCM の RDS 名・tcp_proxy のクラスタ名を読む）。
fn filter_json(f: &p::Filter) -> Value {
    let ty = f
        .typed_config
        .as_ref()
        .map(|a| a.type_url.clone())
        .unwrap_or_default();
    let tc = if ty == p::T_HCM {
        let hcm: Option<p::HttpConnectionManager> = f
            .typed_config
            .as_ref()
            .and_then(|a| p::decode_any(a, p::T_HCM));
        let mut v = json!({"@type": ty});
        if let Some(h) = hcm {
            v["stat_prefix"] = json!(h.stat_prefix);
            match &h.route_specifier {
                Some(p::hcm::RouteSpecifier::Rds(r)) => {
                    v["rds"] = json!({"route_config_name": r.route_config_name, "config_source": {"ads": {}}})
                }
                Some(p::hcm::RouteSpecifier::RouteConfig(rc)) => v["route_config"] = route_json(rc),
                None => {}
            }
            v["http_filters"] = Value::Array(
                h.http_filters
                    .iter()
                    .map(|hf| json!({"name": hf.name, "typed_config": opaque_typed(hf.typed_config.as_ref())}))
                    .collect(),
            );
        }
        v
    } else if ty == p::T_TCP_PROXY {
        let tp: Option<p::TcpProxy> = f
            .typed_config
            .as_ref()
            .and_then(|a| p::decode_any(a, p::T_TCP_PROXY));
        let mut v = json!({"@type": ty});
        if let Some(t) = tp {
            v["stat_prefix"] = json!(t.stat_prefix);
            if let Some(p::tcp_proxy::ClusterSpecifier::Cluster(c)) = &t.cluster_specifier {
                v["cluster"] = json!(c);
            }
        }
        v
    } else {
        opaque_typed(f.typed_config.as_ref())
    };
    json!({"name": f.name, "typed_config": tc})
}

/// 中身を出さない typed_config（protojson で読めるよう TypedStruct に包む）。
fn opaque_typed(a: Option<&p::Any>) -> Value {
    let inner = a.map(|a| a.type_url.clone()).unwrap_or_default();
    json!({"@type": p::T_TYPED_STRUCT_UDPA, "type_url": inner, "value": {}})
}

fn listener_json(l: &p::Listener) -> Value {
    let chains: Vec<Value> = l
        .filter_chains
        .iter()
        .map(|fc| {
            let m = fc.filter_chain_match.clone().unwrap_or_default();
            let mut mj = serde_json::Map::new();
            if let Some(dp) = &m.destination_port {
                mj.insert("destination_port".into(), json!(dp.value));
            }
            if !m.transport_protocol.is_empty() {
                mj.insert("transport_protocol".into(), json!(m.transport_protocol));
            }
            if !m.application_protocols.is_empty() {
                mj.insert(
                    "application_protocols".into(),
                    json!(m.application_protocols),
                );
            }
            if !m.server_names.is_empty() {
                mj.insert("server_names".into(), json!(m.server_names));
            }
            let mut fcj = json!({
                "filter_chain_match": Value::Object(mj),
                "filters": fc.filters.iter().map(filter_json).collect::<Vec<_>>(),
            });
            if !fc.name.is_empty() {
                fcj["name"] = json!(fc.name);
            }
            fcj
        })
        .collect();
    let mut v = json!({
        "@type": p::TYPE_LISTENER,
        "name": l.name,
        "address": addr_json(&l.address),
        "filter_chains": chains,
    });
    if let Some(b) = &l.use_original_dst {
        v["use_original_dst"] = json!(b.value);
    }
    if let Some(b) = &l.bind_to_port {
        v["bind_to_port"] = json!(b.value);
    }
    v
}

fn cluster_type(t: i32) -> &'static str {
    match t {
        1 => "STRICT_DNS",
        2 => "LOGICAL_DNS",
        3 => "EDS",
        4 => "ORIGINAL_DST",
        _ => "STATIC",
    }
}

fn cluster_json(c: &p::Cluster) -> Value {
    let mut v = json!({
        "@type": p::TYPE_CLUSTER,
        "name": c.name,
        "type": cluster_type(c.r#type),
    });
    if let Some(e) = &c.eds_cluster_config {
        v["eds_cluster_config"] =
            json!({"service_name": e.service_name, "eds_config": {"ads": {}}});
    }
    if let Some(t) = &c.connect_timeout {
        v["connect_timeout"] = json!(format!("{}s", t.seconds));
    }
    v
}

fn route_json(r: &p::RouteConfiguration) -> Value {
    json!({
        "@type": p::TYPE_ROUTE,
        "name": r.name,
        "virtual_hosts": r.virtual_hosts.iter().map(|vh| json!({
            "name": vh.name,
            "domains": vh.domains,
            "routes": vh.routes.iter().map(|rt| {
                let mut m = serde_json::Map::new();
                match rt.r#match.as_ref().and_then(|m| m.path_specifier.as_ref()) {
                    Some(p::route_match::PathSpecifier::Prefix(s)) => { m.insert("prefix".into(), json!(s)); }
                    Some(p::route_match::PathSpecifier::Path(s)) => { m.insert("path".into(), json!(s)); }
                    Some(p::route_match::PathSpecifier::SafeRegex(r)) => { m.insert("safe_regex".into(), json!({"regex": r.regex})); }
                    _ => { m.insert("prefix".into(), json!("/")); }
                }
                let mut rj = json!({"name": rt.name, "match": Value::Object(m)});
                if let Some(p::route::Action::Route(ra)) = &rt.action {
                    match &ra.cluster_specifier {
                        Some(p::route_action::ClusterSpecifier::Cluster(c)) => rj["route"] = json!({"cluster": c}),
                        Some(p::route_action::ClusterSpecifier::WeightedClusters(w)) => rj["route"] = json!({
                            "weighted_clusters": {"clusters": w.clusters.iter().map(|c| json!({"name": c.name, "weight": c.weight.as_ref().map(|w| w.value).unwrap_or(0)})).collect::<Vec<_>>()}
                        }),
                        _ => {}
                    }
                }
                rj
            }).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

fn endpoints_json(la: &p::ClusterLoadAssignment) -> Value {
    json!({
        "@type": p::TYPE_ENDPOINT,
        "cluster_name": la.cluster_name,
        "endpoints": la.endpoints.iter().map(|l| json!({
            "lb_endpoints": l.lb_endpoints.iter().map(|e| json!({
                "endpoint": {"address": addr_json(&e.endpoint.as_ref().and_then(|x| x.address.clone()))},
                "health_status": match e.health_status { 1 => "HEALTHY", 2 => "UNHEALTHY", 3 => "DRAINING", 4 => "TIMEOUT", 5 => "DEGRADED", _ => "UNKNOWN" },
                "load_balancing_weight": e.load_balancing_weight.as_ref().map(|w| w.value).unwrap_or(1),
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

/// `config_dump`（`?resource=` は無視して全部を出す。`include_eds` で EDS も）。
pub fn config_dump(query: &str) -> String {
    let raw = RAW.lock().unwrap_or_else(|e| e.into_inner());
    let mut names: Vec<&String> = raw.listeners.keys().collect();
    names.sort();
    let listeners: Vec<Value> = names
        .iter()
        .map(|n| {
            let (v, l) = &raw.listeners[*n];
            json!({"name": n, "active_state": {"version_info": v, "listener": listener_json(l)}})
        })
        .collect();
    let mut cnames: Vec<&String> = raw.clusters.keys().collect();
    cnames.sort();
    let clusters: Vec<Value> = cnames
        .iter()
        .map(|n| {
            let (v, c) = &raw.clusters[*n];
            json!({"version_info": v, "cluster": cluster_json(c)})
        })
        .collect();
    let mut rnames: Vec<&String> = raw.routes.keys().collect();
    rnames.sort();
    let routes: Vec<Value> = rnames
        .iter()
        .map(|n| {
            let (v, r) = &raw.routes[*n];
            json!({"version_info": v, "route_config": route_json(r)})
        })
        .collect();
    let mut configs = vec![
        json!({"@type": "type.googleapis.com/envoy.admin.v3.BootstrapConfigDump", "bootstrap": {
            "node": {"id": crate::mesh::peer::local().map(|l| l.id.clone()).unwrap_or_default()}
        }}),
        json!({"@type": "type.googleapis.com/envoy.admin.v3.ClustersConfigDump", "dynamic_active_clusters": clusters}),
        json!({"@type": "type.googleapis.com/envoy.admin.v3.ListenersConfigDump", "dynamic_listeners": listeners}),
        json!({"@type": "type.googleapis.com/envoy.admin.v3.RoutesConfigDump", "dynamic_route_configs": routes}),
    ];
    if query.contains("include_eds") {
        let mut enames: Vec<&String> = raw.endpoints.keys().collect();
        enames.sort();
        let eps: Vec<Value> = enames
            .iter()
            .map(|n| {
                let (v, e) = &raw.endpoints[*n];
                json!({"version_info": v, "endpoint_config": endpoints_json(e)})
            })
            .collect();
        configs.push(json!({"@type": "type.googleapis.com/envoy.admin.v3.EndpointsConfigDump", "dynamic_endpoint_configs": eps}));
    }
    let b64 = |b: &[u8]| crate::mesh::httpcore::base64_encode(b);
    let secrets: Vec<Value> = crate::mesh::identity::pems()
        .into_iter()
        .map(|(n, chain, ca)| {
            let mut sec = json!({"@type": p::TYPE_SECRET, "name": n});
            if let Some(c) = chain {
                sec["tls_certificate"] = json!({
                    "certificate_chain": {"inline_bytes": b64(&c)},
                    "private_key": {"inline_string": "[redacted]"}
                });
            }
            if let Some(ca) = ca {
                sec["validation_context"] = json!({"trusted_ca": {"inline_bytes": b64(&ca)}});
            }
            json!({"name": n, "version_info": "", "secret": sec})
        })
        .collect();
    configs.push(json!({"@type": "type.googleapis.com/envoy.admin.v3.SecretsConfigDump", "dynamic_active_secrets": secrets}));
    json!({"configs": configs}).to_string()
}

/// `/clusters?format=json`（istioctl `proxy-config endpoints`）。
pub fn clusters_json() -> String {
    let reg = crate::mesh::state::state()
        .registry
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut names: Vec<&String> = reg.clusters.keys().collect();
    names.sort();
    let statuses: Vec<Value> = names
        .iter()
        .map(|n| {
            let slot = &reg.clusters[*n];
            let hosts: Vec<Value> = slot
                .endpoints
                .load()
                .endpoints
                .iter()
                .map(|e| {
                    json!({
                        "address": {"socket_address": {"address": e.addr.ip().to_string(), "port_value": e.addr.port()}},
                        "health_status": {"eds_health_status": if e.healthy { "HEALTHY" } else { "UNHEALTHY" }},
                        "weight": e.weight,
                        "locality": {"region": e.locality.0, "zone": e.locality.1, "sub_zone": e.locality.2},
                    })
                })
                .collect();
            json!({"name": n, "host_statuses": hosts})
        })
        .collect();
    json!({"cluster_statuses": statuses}).to_string()
}

/// `/clusters`（テキスト。`<cluster>::<addr>::health_flags::healthy` の形）。
pub fn clusters_text() -> String {
    let reg = crate::mesh::state::state()
        .registry
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut out = String::new();
    let mut names: Vec<&String> = reg.clusters.keys().collect();
    names.sort();
    for n in names {
        let slot = &reg.clusters[n];
        for e in &slot.endpoints.load().endpoints {
            out.push_str(&format!(
                "{}::{}::health_flags::{}\n{}::{}::weight::{}\n",
                n,
                e.addr,
                if e.healthy {
                    "healthy"
                } else {
                    "/failed_eds_health"
                },
                n,
                e.addr,
                e.weight
            ));
        }
    }
    out
}

pub fn listeners_text() -> String {
    let t = crate::mesh::state::state().listeners.load();
    let mut out = String::new();
    for l in t
        .bound
        .iter()
        .chain(t.by_addr.values())
        .chain(t.by_port.values())
    {
        out.push_str(&format!("{}::{}\n", l.name, l.addr));
    }
    out
}
