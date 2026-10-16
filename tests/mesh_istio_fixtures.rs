//! istiod が生成した xDS のフィクスチャの変換（F-222）。
//!
//! `tools/k8s/mesh-fixtures.sh` が kind の Istio から保存した Delta ADS の応答（`tests/fixtures/istio/<版>/`）を
//! 受け取った順に変換し、veil が全部 ACK できること（変換の失敗 = NACK が無いこと）と、代表的な設定が
//! メッシュモデルに入っていることを確かめる。Istio の版を上げたらフィクスチャを取り直してこの試験を通す。
//!
//! `cargo test --test mesh_istio_fixtures --features mesh-istio`
#![cfg(feature = "mesh-istio")]
// 理由付き allow: テストはフィクスチャを同期 I/O で読む（データプレーンの外）。
#![allow(clippy::disallowed_methods)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use prost::Message;
use veil::mesh::model::*;
use veil::mesh::state::Registry;
use veil::mesh::xds::proto::{self as p, decode_any};
use veil::mesh::xds::translate;

const VERSIONS: &[&str] = &["1.31.1"];

/// 1 つのサイドカーが受け取った応答を順に変換した結果。
#[derive(Default)]
struct Applied {
    reg: Registry,
    listeners: HashMap<String, Arc<MeshListener>>,
    clusters: HashMap<String, Arc<ClusterCfg>>,
    endpoints: HashMap<String, EndpointSet>,
    errors: Vec<String>,
    responses: usize,
}

fn dir(version: &str, app: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/istio")
        .join(version)
        .join(app)
}

fn apply(version: &str, app: &str) -> Applied {
    let d = dir(version, app);
    let mut files: Vec<PathBuf> = std::fs::read_dir(&d)
        .unwrap_or_else(|e| panic!("{}: {}", d.display(), e))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "pb"))
        .collect();
    files.sort();
    let mut a = Applied::default();
    for f in files {
        let resp = p::DeltaDiscoveryResponse::decode(&std::fs::read(&f).unwrap()[..])
            .unwrap_or_else(|e| panic!("{}: {}", f.display(), e));
        a.responses += 1;
        for r in &resp.resources {
            let Some(any) = &r.resource else { continue };
            let name = r.name.clone();
            match resp.type_url.as_str() {
                p::TYPE_CLUSTER => match decode_any::<p::Cluster>(any, p::TYPE_CLUSTER) {
                    Some(c) => match translate::cluster(&c) {
                        Ok(cfg) => {
                            a.clusters.insert(name, Arc::new(cfg));
                        }
                        Err(e) => a.errors.push(format!("cluster {}: {}", name, e)),
                    },
                    None => a.errors.push(format!("cluster {}: cannot decode", name)),
                },
                p::TYPE_LISTENER => match decode_any::<p::Listener>(any, p::TYPE_LISTENER) {
                    Some(l) => match translate::listener(&l, &mut a.reg) {
                        Ok(ml) => {
                            a.listeners.insert(name, Arc::new(ml));
                        }
                        Err(e) => a.errors.push(format!("listener {}: {}", name, e)),
                    },
                    None => a.errors.push(format!("listener {}: cannot decode", name)),
                },
                p::TYPE_ROUTE => match decode_any::<p::RouteConfiguration>(any, p::TYPE_ROUTE) {
                    Some(rc) => match translate::route_table(&rc, &mut a.reg) {
                        Ok(t) => {
                            let slot = a.reg.route_slot(&name);
                            slot.table.store(Arc::new(t));
                        }
                        Err(e) => a.errors.push(format!("route {}: {}", name, e)),
                    },
                    None => a.errors.push(format!("route {}: cannot decode", name)),
                },
                p::TYPE_ENDPOINT => {
                    match decode_any::<p::ClusterLoadAssignment>(any, p::TYPE_ENDPOINT) {
                        Some(la) => {
                            a.endpoints
                                .insert(name, translate::endpoint_set(&la, false));
                        }
                        None => a.errors.push(format!("endpoints {}: cannot decode", name)),
                    }
                }
                // ECDS 等（この設定では来ない。来ても変換の対象外）
                _ => {}
            }
        }
        for name in &resp.removed_resources {
            match resp.type_url.as_str() {
                p::TYPE_CLUSTER => {
                    a.clusters.remove(name);
                }
                p::TYPE_LISTENER => {
                    a.listeners.remove(name);
                }
                _ => {}
            }
        }
    }
    a
}

fn route_tables(a: &Applied) -> Vec<Arc<RouteTable>> {
    a.reg.routes.values().map(|s| s.table.load_full()).collect()
}

fn find_vhost<'a>(tables: &'a [Arc<RouteTable>], domain: &str) -> Option<&'a VirtualHost> {
    tables
        .iter()
        .flat_map(|t| t.vhosts.iter())
        .find(|v| v.name.starts_with(domain))
}

#[test]
fn istio_outbound_config_translates_without_nack() {
    for v in VERSIONS {
        let a = apply(v, "client");
        assert!(a.responses > 0, "{v}: no responses");
        assert!(a.errors.is_empty(), "{v}: NACK: {:#?}", a.errors);
        assert!(a.listeners.contains_key("virtualOutbound"), "{v}");
        assert!(a.listeners.contains_key("virtualInbound"), "{v}");
        assert!(!a.endpoints.is_empty(), "{v}: no EDS");

        // DestinationRule: サブセットのクラスタと LEAST_REQUEST・外れ値検出・接続の上限
        let v1 = a
            .clusters
            .iter()
            .find(|(n, _)| n.starts_with("outbound|8080|v1|fortio.xds-fixtures."))
            .map(|(_, c)| c.clone())
            .unwrap_or_else(|| panic!("{v}: no v1 subset cluster"));
        assert_eq!(v1.lb, LbPolicy::LeastRequest, "{v}");
        assert!(v1.outlier.is_some(), "{v}: outlierDetection");
        assert_eq!(v1.max_connections, 100, "{v}");
        assert_eq!(v1.max_requests_per_conn, 10, "{v}");
        assert!(
            v1.tls_default.is_some() || !v1.tls_matches.is_empty(),
            "{v}: auto mTLS"
        );
        // 上流の ID（auto mTLS の SAN の期待値 = 宛先のサービスアカウント）
        let tls = v1
            .tls_matches
            .iter()
            .find_map(|(_, t)| t.clone())
            .or(v1.tls_default.clone());
        assert_eq!(
            tls.and_then(|t| t.principal.clone()).as_deref(),
            Some("spiffe://cluster.local/ns/xds-fixtures/sa/default"),
            "{v}: upstream principal"
        );
        // whoami の一貫性ハッシュ
        let who = a
            .clusters
            .iter()
            .find(|(n, _)| n.starts_with("outbound|80||whoami.xds-fixtures."))
            .map(|(_, c)| c.clone())
            .unwrap_or_else(|| panic!("{v}: no whoami cluster"));
        assert!(
            matches!(who.lb, LbPolicy::RingHash | LbPolicy::Maglev),
            "{v}: {:?}",
            who.lb
        );

        // VirtualService: ヘッダの一致・フォルト・正規表現の書き換え・重み付き分割・リトライ・タイムアウト・ミラー・CORS
        let tables = route_tables(&a);
        let vh = find_vhost(&tables, "fortio.xds-fixtures.")
            .unwrap_or_else(|| panic!("{v}: no fortio vhost"));
        assert!(vh.routes.len() >= 4, "{v}: {} routes", vh.routes.len());
        assert!(
            vh.routes[0].headers.iter().any(|h| h.name == "x-canary"),
            "{v}: header match"
        );
        assert!(vh.routes[1].fault.is_some(), "{v}: fault");
        let f = vh.routes[1].fault.as_ref().unwrap();
        assert!(f.delay.is_some() && f.abort_http.is_some(), "{v}: {:?}", f);
        let RouteAction::Forward(fw) = &vh.routes[2].action else {
            panic!("{v}: regex route is not a forward")
        };
        // Istio は正規表現の一致の rewrite.uri を prefix_rewrite で送る（パス全体の置き換え）
        assert!(matches!(vh.routes[2].path, PathMatch::Regex(_)), "{v}");
        assert_eq!(
            fw.prefix_rewrite.as_deref(),
            Some(&b"/echo"[..]),
            "{v}: rewrite"
        );
        let RouteAction::Forward(fw) = &vh.routes[3].action else {
            panic!("{v}: default route is not a forward")
        };
        let ClusterTarget::Weighted { clusters, total } = &fw.target else {
            panic!("{v}: not weighted")
        };
        assert_eq!(*total, 100, "{v}");
        assert_eq!(
            clusters.iter().map(|c| c.weight).collect::<Vec<_>>(),
            vec![80, 20],
            "{v}"
        );
        let r = fw.retry.as_ref().expect("retry");
        assert_eq!(r.num_retries, 3, "{v}");
        assert!(
            r.on_5xx && r.on_reset && r.on_connect_failure,
            "{v}: {:?}",
            r
        );
        assert_eq!(fw.timeout, Some(std::time::Duration::from_secs(3)), "{v}");
        assert_eq!(fw.mirrors.len(), 1, "{v}: mirror");
        assert_eq!(fw.mirrors[0].1, 250_000, "{v}: mirror percentage");
        assert!(
            vh.routes[3].cors.is_some() || vh.cors.is_some(),
            "{v}: CORS"
        );
        assert!(
            !vh.routes[3].request_ops.is_empty(),
            "{v}: header set/remove"
        );

        // ServiceEntry（MESH_EXTERNAL・DNS）
        assert!(
            a.clusters
                .keys()
                .any(|n| n.starts_with("outbound|80||api.example.com")),
            "{v}: ServiceEntry cluster"
        );
    }
}

#[test]
fn istio_inbound_security_config_translates_without_nack() {
    for v in VERSIONS {
        let a = apply(v, "whoami");
        assert!(a.errors.is_empty(), "{v}: NACK: {:#?}", a.errors);
        let inbound = a
            .listeners
            .get("virtualInbound")
            .unwrap_or_else(|| panic!("{v}: no virtualInbound"));
        // STRICT: 平文のチェーンが無く、80 のチェーンはクライアント証明書を要求する
        let chains: Vec<&Arc<FilterChain>> = inbound
            .chains
            .iter()
            .filter(|c| c.m.dst_port == Some(80))
            .collect();
        assert!(!chains.is_empty(), "{v}: no chain for port 80");
        assert!(
            chains
                .iter()
                .all(|c| c.tls.as_ref().is_some_and(|t| t.require_client_cert)),
            "{v}: STRICT mTLS"
        );
        // HTTP の標準メトリクス（HCM の istio.stats）を数える
        assert!(chains.iter().all(|c| c.stats), "{v}: istio.stats");
        // AuthorizationPolicy（DENY と ALLOW）と RequestAuthentication（JWT）が HTTP のフィルタに入っている
        let ChainAction::Http(h) = &chains[0].action else {
            panic!("{v}: not HTTP")
        };
        let mut allow = 0;
        let mut deny = 0;
        let mut jwt = 0;
        for f in &h.filters {
            match f {
                HttpFilterCfg::Rbac(r) if r.action == RbacAction::Allow && !r.shadow => {
                    allow += r.policies.len()
                }
                HttpFilterCfg::Rbac(r) if r.action == RbacAction::Deny && !r.shadow => {
                    deny += r.policies.len()
                }
                HttpFilterCfg::Jwt(_) => jwt += 1,
                _ => {}
            }
        }
        assert!(
            allow >= 1 && deny >= 1 && jwt == 1,
            "{v}: allow={allow} deny={deny} jwt={jwt}"
        );
    }
}
