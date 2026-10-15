//! Envoy の xDS v3 のリソース → メッシュモデル（F-222）。
//!
//! istiod が生成する設定を基準に、扱う型を列挙して変換する。知らない `typed_config` は、任意
//! （`is_optional`）なら無視して記録し、そうでなければそのリソースを拒否する（NACK。`error_detail` に型名）。
//! 認可のフィルタ（RBAC 等）は黙って落とさない（ポリシーの抜けがセキュリティ事故になる。F-220）。

use super::proto::{self as p, decode_any};
use crate::mesh::model::*;
use crate::mesh::state::Registry;
use bytes::Bytes;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

pub type R<T> = Result<T, String>;

// ---------------------------------------------------------------------------
// 共通
// ---------------------------------------------------------------------------

fn dur(d: &Option<p::Duration>) -> Option<Duration> {
    d.as_ref().map(|d| d.to_std())
}

/// 0 は無制限（None）。
fn dur_nonzero(d: &Option<p::Duration>) -> Option<Duration> {
    dur(d).filter(|d| !d.is_zero())
}

pub fn string_matcher(m: &p::StringMatcher) -> R<StringMatcher> {
    let pattern = match &m.pattern {
        Some(p::string_matcher::Pattern::Exact(s)) => StrMatch::Exact(s.clone()),
        Some(p::string_matcher::Pattern::Prefix(s)) => StrMatch::Prefix(s.clone()),
        Some(p::string_matcher::Pattern::Suffix(s)) => StrMatch::Suffix(s.clone()),
        Some(p::string_matcher::Pattern::Contains(s)) => StrMatch::Contains(s.clone()),
        Some(p::string_matcher::Pattern::SafeRegex(r)) => StrMatch::Regex(compile_regex(&r.regex)?),
        None => StrMatch::Prefix(String::new()),
    };
    Ok(StringMatcher {
        pattern,
        ignore_case: m.ignore_case,
    })
}

fn compile_regex(re: &str) -> R<regex::Regex> {
    regex::Regex::new(re).map_err(|e| format!("invalid regex '{}': {}", re, e))
}

pub fn header_match(h: &p::HeaderMatcher) -> R<HeaderMatch> {
    use p::header_matcher::Specifier as S;
    let kind = match &h.specifier {
        Some(S::ExactMatch(s)) => HeaderMatchKind::Str(StringMatcher::exact(s)),
        Some(S::PrefixMatch(s)) => HeaderMatchKind::Str(StringMatcher {
            pattern: StrMatch::Prefix(s.clone()),
            ignore_case: false,
        }),
        Some(S::SuffixMatch(s)) => HeaderMatchKind::Str(StringMatcher {
            pattern: StrMatch::Suffix(s.clone()),
            ignore_case: false,
        }),
        Some(S::ContainsMatch(s)) => HeaderMatchKind::Str(StringMatcher {
            pattern: StrMatch::Contains(s.clone()),
            ignore_case: false,
        }),
        Some(S::SafeRegexMatch(r)) => HeaderMatchKind::Str(StringMatcher {
            pattern: StrMatch::Regex(compile_regex(&r.regex)?),
            ignore_case: false,
        }),
        Some(S::PresentMatch(b)) => HeaderMatchKind::Present(*b),
        Some(S::StringMatch(m)) => HeaderMatchKind::Str(string_matcher(m)?),
        None => HeaderMatchKind::Present(true),
    };
    Ok(HeaderMatch {
        name: h.name.to_ascii_lowercase(),
        kind,
        invert: h.invert_match,
        treat_missing_as_empty: h.treat_missing_header_as_empty,
    })
}

fn header_ops(add: &[p::HeaderValueOption], remove: &[String]) -> HeaderOps {
    HeaderOps {
        remove: remove
            .iter()
            .map(|r| Bytes::from(r.to_ascii_lowercase()))
            .collect(),
        add: add
            .iter()
            .filter_map(|o| {
                let h = o.header.as_ref()?;
                let value = if !h.raw_value.is_empty() {
                    Bytes::from(h.raw_value.clone())
                } else {
                    Bytes::from(h.value.clone())
                };
                let action = match &o.append {
                    Some(b) if b.value => AppendAction::Append,
                    Some(_) => AppendAction::Overwrite,
                    None => match o.append_action {
                        1 => AppendAction::AddIfAbsent,
                        2 => AppendAction::Overwrite,
                        3 => AppendAction::OverwriteIfExists,
                        _ => AppendAction::Append,
                    },
                };
                Some(HeaderAdd {
                    name: Bytes::from(h.key.to_ascii_lowercase()),
                    value,
                    action,
                })
            })
            .collect(),
    }
}

fn cidr(c: &p::CidrRange) -> R<Cidr> {
    let addr = c
        .address_prefix
        .parse()
        .map_err(|_| format!("invalid CIDR prefix '{}'", c.address_prefix))?;
    let len = c
        .prefix_len
        .as_ref()
        .map(|l| l.value as u8)
        .unwrap_or(match addr {
            std::net::IpAddr::V4(_) => 32,
            std::net::IpAddr::V6(_) => 128,
        });
    Ok(Cidr { addr, len })
}

/// `Struct` の文字列フィールドを (k, v) の組にする。
fn struct_strings(s: &p::Struct) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = s
        .fields
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
        .collect();
    v.sort();
    v
}

/// ECDS で届いた HTTP フィルタ（`TypedExtensionConfig.typed_config`）を変換する。
/// Istio の WasmPlugin は Wasm、pilot-agent が取得に失敗して差し替えたものは RBAC（拒否）か何もしないフィルタ。
pub fn ecds_filter(name: &str, any: &p::Any) -> R<HttpFilterCfg> {
    let ty = inner_type(any);
    if ty == p::T_WASM {
        let w = decode_any::<p::WasmFilter>(any, p::T_WASM).ok_or("invalid Wasm filter")?;
        let pc = w.config.unwrap_or_default();
        let fail_open = pc.fail_open || pc.failure_policy == 1;
        return match wasm_plugin(name, &pc, !fail_open) {
            Ok(f) => Ok(f),
            Err(e) if fail_open => {
                ftlog::warn!("[mesh] wasm plugin {} disabled (fail open): {}", name, e);
                Ok(HttpFilterCfg::Noop)
            }
            Err(e) => Err(e),
        };
    }
    if ty.contains("filters.http.rbac") {
        return Ok(HttpFilterCfg::Rbac(Arc::new(super::rbac::http_rbac(any)?)));
    }
    if ty.ends_with("stats.PluginConfig")
        || ty.contains("peer_metadata")
        || ty.contains("metadataexchange")
        || ty.contains("grpc_stats")
    {
        return Ok(HttpFilterCfg::Noop);
    }
    Err(format!("unsupported ECDS filter {} ({})", name, ty))
}

#[cfg(feature = "wasm")]
fn wasm_plugin(name: &str, pc: &p::WasmPluginConfig, fail_closed: bool) -> R<HttpFilterCfg> {
    let vm = pc.vm_config.as_ref().ok_or("Wasm plugin without vm_config")?;
    let path = match vm
        .code
        .as_ref()
        .and_then(|c| c.local.as_ref())
        .and_then(|d| d.specifier.as_ref())
    {
        Some(p::data_source::Specifier::Filename(f)) => f.clone(),
        _ => return Err("Wasm plugin without a local module file".into()),
    };
    let configuration = pc.configuration.as_ref().map(any_bytes).unwrap_or_default();
    let w = crate::mesh::wasm::load(name, &path, &pc.root_id, configuration, fail_closed)?;
    Ok(HttpFilterCfg::Wasm(Arc::new(w)))
}

#[cfg(not(feature = "wasm"))]
fn wasm_plugin(_name: &str, _pc: &p::WasmPluginConfig, _fail_closed: bool) -> R<HttpFilterCfg> {
    Err("this veil is built without Proxy-Wasm (the `wasm` feature)".into())
}

/// Wasm の `configuration`（`StringValue`・`BytesValue`・`Struct`）をプラグインに渡すバイト列にする。
#[cfg(feature = "wasm")]
fn any_bytes(a: &p::Any) -> Vec<u8> {
    if a.type_url.ends_with("google.protobuf.StringValue") {
        return decode_any::<p::StringValue>(a, &a.type_url)
            .map(|v| v.value.into_bytes())
            .unwrap_or_default();
    }
    if a.type_url.ends_with("google.protobuf.BytesValue") {
        return decode_any::<p::BytesValue>(a, &a.type_url)
            .map(|v| v.value)
            .unwrap_or_default();
    }
    if a.type_url.ends_with("google.protobuf.Struct") {
        return decode_any::<p::Struct>(a, &a.type_url)
            .map(|s| super::struct_to_json(&s).to_string().into_bytes())
            .unwrap_or_default();
    }
    a.value.clone()
}

fn is_typed_struct(any: &p::Any) -> bool {
    any.type_url == p::T_TYPED_STRUCT_UDPA || any.type_url == p::T_TYPED_STRUCT_XDS
}

/// TypedStruct の中身（JSON の文字列）。
fn typed_struct_json(any: &p::Any) -> String {
    decode_any::<p::TypedStruct>(any, &any.type_url)
        .and_then(|ts| ts.value)
        .map(|v| super::struct_to_json(&v).to_string())
        .unwrap_or_default()
}

/// Istio の拡張フィルタ（TypedStruct で包まれる）の実際の型 URL。
fn inner_type(any: &p::Any) -> String {
    if any.type_url == p::T_TYPED_STRUCT_UDPA || any.type_url == p::T_TYPED_STRUCT_XDS {
        if let Some(ts) = decode_any::<p::TypedStruct>(any, &any.type_url) {
            return ts.type_url;
        }
    }
    any.type_url.clone()
}

// ---------------------------------------------------------------------------
// TLS
// ---------------------------------------------------------------------------

/// SAN の条件（`match_subject_alt_names` と `match_typed_subject_alt_names`）。
fn san_matchers(v: Option<&p::CertificateValidationContext>) -> R<Vec<StringMatcher>> {
    let mut out = Vec::new();
    if let Some(v) = v {
        for m in &v.match_subject_alt_names {
            out.push(string_matcher(m)?);
        }
        for m in &v.match_typed_subject_alt_names {
            if let Some(sm) = &m.matcher {
                out.push(string_matcher(sm)?);
            }
        }
    }
    Ok(out)
}

/// 共通の TLS 設定から（自分の証明書の名前・信頼アンカーの名前・SAN の条件・ALPN）。
fn common_tls(c: &p::CommonTlsContext) -> R<(Option<String>, Option<String>, Vec<StringMatcher>)> {
    // 参照された秘密（ゲートウェイの credentialName・DestinationRule の credentialName 等）を SDS で購読する
    for sc in &c.tls_certificate_sds_secret_configs {
        crate::mesh::envoy::sds::want(&sc.name);
    }
    use p::common_tls_context::ValidationContextType as V0;
    match &c.validation_context_type {
        Some(V0::CombinedValidationContext(cv)) => {
            if let Some(s) = &cv.validation_context_sds_secret_config {
                crate::mesh::envoy::sds::want(&s.name);
            }
        }
        Some(V0::ValidationContextSdsSecretConfig(s)) => crate::mesh::envoy::sds::want(&s.name),
        _ => {}
    }
    let cert = c
        .tls_certificate_sds_secret_configs
        .first()
        .map(|s| s.name.clone());
    use p::common_tls_context::ValidationContextType as V;
    let (roots, sans) = match &c.validation_context_type {
        Some(V::CombinedValidationContext(cv)) => (
            cv.validation_context_sds_secret_config
                .as_ref()
                .map(|s| s.name.clone()),
            san_matchers(cv.default_validation_context.as_ref())?,
        ),
        Some(V::ValidationContextSdsSecretConfig(s)) => (Some(s.name.clone()), Vec::new()),
        Some(V::ValidationContext(v)) => {
            // インラインの信頼アンカー（ゲートウェイの DestinationRule 等）
            let name = match v.trusted_ca.as_ref().and_then(|d| d.inline()) {
                Some(pem) => {
                    let n = format!("inline:{}", crate::mesh::identity::cert_sha256_hex(pem));
                    crate::mesh::identity::set_roots_pem(&n, pem)?;
                    Some(n)
                }
                None => None,
            };
            (name, san_matchers(Some(v))?)
        }
        None => (None, Vec::new()),
    };
    Ok((cert, roots, sans))
}

fn downstream_tls(ctx: &p::DownstreamTlsContext) -> R<Arc<DownstreamTls>> {
    let common = ctx.common_tls_context.clone().unwrap_or_default();
    let (cert, roots, sans) = common_tls(&common)?;
    let cert_name = cert.unwrap_or_else(|| "default".to_string());
    let require = ctx
        .require_client_certificate
        .as_ref()
        .map(|b| b.value)
        .unwrap_or(false);
    // istio-peer-exchange（TCP の metadata exchange）は交渉しない（交換を実装するまで。交渉すると
    // Envoy のクライアントがストリームの先頭に交換のヘッダを書く）
    let alpn: Vec<Vec<u8>> = common
        .alpn_protocols
        .iter()
        .filter(|a| a.as_str() != "istio-peer-exchange")
        .map(|a| a.as_bytes().to_vec())
        .collect();
    let server_config = crate::mesh::identity::server_config(
        &cert_name,
        roots.as_deref(),
        sans.clone(),
        require,
        &alpn,
    )?;
    let server_config_no_alpn = crate::mesh::identity::server_config(
        &cert_name,
        roots.as_deref(),
        sans.clone(),
        require,
        &[],
    )?;
    Ok(Arc::new(DownstreamTls {
        require_client_cert: require,
        alpn,
        san_matchers: sans,
        cert_name,
        validation_name: roots,
        server_config,
        server_config_no_alpn,
    }))
}

pub fn upstream_tls(ctx: &p::UpstreamTlsContext) -> R<Arc<UpstreamTls>> {
    let common = ctx.common_tls_context.clone().unwrap_or_default();
    let (cert, roots, sans) = common_tls(&common)?;
    let xds_alpn: Vec<&[u8]> = common.alpn_protocols.iter().map(|a| a.as_bytes()).collect();
    let mesh_mtls = common
        .alpn_protocols
        .iter()
        .any(|a| a == "istio" || a == "istio-peer-exchange");
    // TCP: istio-peer-exchange は提示しない（受け側の Envoy が metadata exchange を始めるため。
    // 交換を実装するまでは `istio` だけでフィルタチェーンに一致させる）
    let tcp_alpn: Vec<&[u8]> = xds_alpn
        .iter()
        .copied()
        .filter(|a| *a != b"istio-peer-exchange")
        .collect();
    let (h1, h2): (Vec<&[u8]>, Vec<&[u8]>) = if mesh_mtls {
        (
            vec![b"istio-http/1.1", b"istio", b"http/1.1"],
            vec![b"istio-h2", b"istio", b"h2"],
        )
    } else if xds_alpn.is_empty() {
        (vec![b"http/1.1"], vec![b"h2", b"http/1.1"])
    } else {
        (xds_alpn.clone(), xds_alpn.clone())
    };
    let mk = |alpn: &[&[u8]]| {
        crate::mesh::identity::client_config(cert.as_deref(), roots.as_deref(), sans.clone(), alpn)
    };
    Ok(Arc::new(UpstreamTls {
        sni: ctx.sni.clone(),
        san_matchers: sans.clone(),
        tcp: mk(&tcp_alpn)?,
        h1: mk(&h1)?,
        h2: mk(&h2)?,
        mesh_mtls,
        transport: None,
    }))
}

fn transport_socket_upstream(ts: &p::TransportSocket) -> R<Option<Arc<UpstreamTls>>> {
    let Some(any) = &ts.typed_config else {
        return Ok(None);
    };
    if any.type_url == p::T_UPSTREAM_TLS {
        let ctx: p::UpstreamTlsContext =
            decode_any(any, p::T_UPSTREAM_TLS).ok_or("invalid UpstreamTlsContext")?;
        return Ok(Some(upstream_tls(&ctx)?));
    }
    if any.type_url.ends_with("RawBuffer") {
        return Ok(None);
    }
    if any.type_url == p::T_INTERNAL_UPSTREAM {
        // 内部リスナーへの接続（ambient の waypoint）。中の transport socket を使う
        let iu: p::InternalUpstreamTransport =
            decode_any(any, p::T_INTERNAL_UPSTREAM).ok_or("invalid InternalUpstreamTransport")?;
        return match &iu.transport_socket {
            Some(ts) => transport_socket_upstream(ts),
            None => Ok(None),
        };
    }
    Err(format!(
        "unsupported upstream transport socket {}",
        any.type_url
    ))
}

// ---------------------------------------------------------------------------
// リスナー
// ---------------------------------------------------------------------------

pub fn listener(l: &p::Listener, reg: &mut Registry) -> R<MeshListener> {
    let internal = l.internal_listener.is_some();
    let addr = match l.address.as_ref().and_then(|a| a.socket_addr()) {
        Some(a) => a,
        // 内部リスナーはアドレスを持たない（名前で引く）
        None if internal => SocketAddr::from(([0, 0, 0, 0], 0)),
        None => return Err(format!("listener {}: unsupported address", l.name)),
    };
    let direction = match l.traffic_direction {
        1 => Direction::Inbound,
        2 => Direction::Outbound,
        _ if l.name == "virtualInbound" => Direction::Inbound,
        _ if l.name == "virtualOutbound" => Direction::Outbound,
        _ => Direction::Unspecified,
    };
    let mut tls_inspector = false;
    let mut http_inspector = false;
    for lf in &l.listener_filters {
        match lf.name.as_str() {
            "envoy.filters.listener.tls_inspector" => tls_inspector = true,
            "envoy.filters.listener.http_inspector" => http_inspector = true,
            "envoy.filters.listener.original_dst" | "envoy.filters.listener.proxy_protocol" => {}
            other => {
                let t = lf.typed_config.as_ref().map(inner_type).unwrap_or_default();
                if t.contains("tls_inspector") {
                    tls_inspector = true;
                } else if t.contains("http_inspector") {
                    http_inspector = true;
                } else if !t.contains("original_dst") {
                    ftlog::debug!(
                        "[mesh] listener {}: ignoring listener filter {}",
                        l.name,
                        other
                    );
                }
            }
        }
    }
    let mut chains = Vec::with_capacity(l.filter_chains.len());
    for fc in &l.filter_chains {
        chains.push(Arc::new(
            filter_chain(fc, reg, direction).map_err(|e| format!("listener {}: {}", l.name, e))?,
        ));
    }
    let default_chain = match &l.default_filter_chain {
        Some(fc) => Some(Arc::new(
            filter_chain(fc, reg, direction).map_err(|e| format!("listener {}: {}", l.name, e))?,
        )),
        None => None,
    };
    let matcher = match &l.filter_chain_matcher {
        Some(m) => Some(
            chain_matcher(m, &l.filter_chains).map_err(|e| format!("listener {}: {}", l.name, e))?,
        ),
        None => None,
    };
    let needs_sniff = tls_inspector
        || http_inspector
        || matcher.is_some()
        || chains.iter().any(|c| {
            c.m.transport.is_some()
                || !c.m.application_protocols.is_empty()
                || !c.m.server_names.is_empty()
        });
    Ok(MeshListener {
        name: l.name.clone(),
        addr,
        use_original_dst: l
            .use_original_dst
            .as_ref()
            .map(|b| b.value)
            .unwrap_or(false),
        bind_to_port: !internal && l.bind_to_port.as_ref().map(|b| b.value).unwrap_or(true),
        direction,
        chains,
        default_chain,
        tls_inspector,
        http_inspector,
        needs_sniff,
        internal,
        matcher,
    })
}

/// `filter_chain_matcher`（matcher_tree）を変換する。アクションはフィルタチェーンの名前（`StringValue`）。
fn chain_matcher(m: &p::XdsMatcher, chains: &[p::FilterChain]) -> R<ChainMatcher> {
    let tree = match &m.matcher_type {
        Some(p::xds_matcher::MatcherType::MatcherTree(t)) => t,
        Some(p::xds_matcher::MatcherType::MatcherList(_)) => {
            return Err("filter_chain_matcher: matcher_list is not supported".into());
        }
        None => return Err("filter_chain_matcher without a matcher".into()),
    };
    let input_ty = tree
        .input
        .as_ref()
        .and_then(|i| i.typed_config.as_ref())
        .map(|a| a.type_url.as_str())
        .unwrap_or("");
    let input = match input_ty.rsplit('.').next().unwrap_or("") {
        "DestinationIPInput" => MatchInput::DstIp,
        "DestinationPortInput" => MatchInput::DstPort,
        "SourceIPInput" | "DirectSourceIPInput" => MatchInput::SrcIp,
        "SourcePortInput" => MatchInput::SrcPort,
        "ServerNameInput" => MatchInput::ServerName,
        "TransportProtocolInput" => MatchInput::TransportProtocol,
        "ApplicationProtocolInput" => MatchInput::ApplicationProtocol,
        _ => return Err(format!("filter_chain_matcher: unsupported input {}", input_ty)),
    };
    let on = |o: &p::XdsOnMatch| on_match(o, chains);
    let map = match &tree.tree_type {
        Some(p::xds_matcher::TreeType::ExactMatchMap(mm)) => {
            let mut h = HashMap::new();
            for (k, v) in &mm.map {
                h.insert(k.clone(), on(v)?);
            }
            MatchMap::Exact(h)
        }
        Some(p::xds_matcher::TreeType::PrefixMatchMap(mm)) => MatchMap::Prefix(
            mm.map
                .iter()
                .map(|(k, v)| Ok((k.clone(), on(v)?)))
                .collect::<R<_>>()?,
        ),
        Some(p::xds_matcher::TreeType::CustomMatch(c)) => {
            let any = c.typed_config.as_ref().ok_or("custom_match without config")?;
            let ipm: p::XdsIpMatcher = decode_any(any, p::T_IP_MATCHER)
                .ok_or_else(|| format!("unsupported custom_match {}", any.type_url))?;
            let mut v = Vec::new();
            for rm in &ipm.range_matchers {
                let Some(o) = &rm.on_match else { continue };
                v.push((rm.ranges.iter().map(cidr).collect::<R<_>>()?, on(o)?));
            }
            MatchMap::Ip(v)
        }
        None => return Err("matcher_tree without a map".into()),
    };
    let on_no_match = match &m.on_no_match {
        Some(o) => Some(Box::new(on(o)?)),
        None => None,
    };
    Ok(ChainMatcher {
        input,
        map,
        on_no_match,
    })
}

fn on_match(o: &p::XdsOnMatch, chains: &[p::FilterChain]) -> R<ChainOnMatch> {
    match &o.on_match {
        Some(p::xds_matcher::OnMatch::Matcher(m)) => {
            Ok(ChainOnMatch::Matcher(Box::new(chain_matcher(m, chains)?)))
        }
        Some(p::xds_matcher::OnMatch::Action(a)) => {
            let name = match &a.typed_config {
                Some(any) if any.type_url.ends_with("google.protobuf.StringValue") => {
                    decode_any::<p::StringValue>(any, &any.type_url)
                        .map(|v| v.value)
                        .unwrap_or_else(|| a.name.clone())
                }
                _ => a.name.clone(),
            };
            chains
                .iter()
                .position(|c| c.name == name)
                .map(ChainOnMatch::Chain)
                .ok_or_else(|| format!("filter_chain_matcher: no filter chain named {}", name))
        }
        None => Err("on_match without an action".into()),
    }
}

fn chain_match(m: &p::FilterChainMatch) -> R<ChainMatch> {
    Ok(ChainMatch {
        dst_port: m.destination_port.as_ref().map(|p| p.value as u16),
        prefix_ranges: m.prefix_ranges.iter().map(cidr).collect::<R<_>>()?,
        server_names: m
            .server_names
            .iter()
            .map(|s| s.to_ascii_lowercase())
            .collect(),
        transport: match m.transport_protocol.as_str() {
            "tls" => Some(TransportProto::Tls),
            "raw_buffer" => Some(TransportProto::RawBuffer),
            "" => None,
            other => return Err(format!("unsupported transport_protocol {}", other)),
        },
        application_protocols: m.application_protocols.clone(),
        source_prefix_ranges: m.source_prefix_ranges.iter().map(cidr).collect::<R<_>>()?,
        source_ports: m.source_ports.iter().map(|p| *p as u16).collect(),
    })
}

/// 何もしないネットワークフィルタ（観測・peer exchange 等。veil が別の仕組みで行う）。
fn is_passive_network_filter(name: &str, ty: &str) -> bool {
    matches!(
        name,
        "istio.metadata_exchange" | "istio.stats" | "envoy.filters.network.connection_limit"
    ) || ty.contains("metadataexchange")
        || ty.ends_with("stats.PluginConfig")
        // waypoint の filter state（宛先・相手の ID の受け渡し）。veil は接続の文脈で直接渡す
        || ty.contains("set_filter_state")
}

fn filter_chain(fc: &p::FilterChain, reg: &mut Registry, direction: Direction) -> R<FilterChain> {
    let m = match &fc.filter_chain_match {
        Some(m) => chain_match(m)?,
        None => ChainMatch::default(),
    };
    let tls = match &fc.transport_socket {
        Some(ts) => match &ts.typed_config {
            Some(any) if any.type_url == p::T_DOWNSTREAM_TLS => {
                let ctx: p::DownstreamTlsContext =
                    decode_any(any, p::T_DOWNSTREAM_TLS).ok_or("invalid DownstreamTlsContext")?;
                Some(downstream_tls(&ctx)?)
            }
            Some(any) if any.type_url.ends_with("RawBuffer") => None,
            None => None,
            Some(any) => return Err(format!("unsupported transport socket {}", any.type_url)),
        },
        None => None,
    };
    let mut action = None;
    let mut stats = false;
    let mut tcp_rbac: Option<Arc<RbacCfg>> = None;
    for f in &fc.filters {
        let ty = f.typed_config.as_ref().map(inner_type).unwrap_or_default();
        if f.name == "istio.stats" || ty.ends_with("stats.PluginConfig") {
            stats = true;
            continue;
        }
        if is_passive_network_filter(&f.name, &ty) {
            continue;
        }
        if ty == p::T_HCM || f.name == "envoy.filters.network.http_connection_manager" {
            let any = f.typed_config.as_ref().ok_or("HCM without config")?;
            let hcm: p::HttpConnectionManager =
                decode_any(any, p::T_HCM).ok_or("invalid HttpConnectionManager")?;
            action = Some(ChainAction::Http(Arc::new(http_cm(
                &hcm,
                reg,
                direction,
                tcp_rbac.take(),
            )?)));
            continue;
        }
        if ty == p::T_TCP_PROXY || f.name == "envoy.filters.network.tcp_proxy" {
            let any = f.typed_config.as_ref().ok_or("tcp_proxy without config")?;
            let tp: p::TcpProxy = decode_any(any, p::T_TCP_PROXY).ok_or("invalid TcpProxy")?;
            action = Some(ChainAction::Tcp(Arc::new(tcp_proxy(
                &tp,
                reg,
                tcp_rbac.take(),
            )?)));
            continue;
        }
        if ty.contains("network.rbac") || f.name == "envoy.filters.network.rbac" {
            let any = f.typed_config.as_ref().ok_or("rbac without config")?;
            tcp_rbac = Some(Arc::new(super::rbac::network_rbac(any)?));
            continue;
        }
        return Err(format!("unsupported network filter {} ({})", f.name, ty));
    }
    Ok(FilterChain {
        name: fc.name.clone(),
        m,
        tls,
        action: action.unwrap_or(ChainAction::Close),
        stats,
    })
}

fn tcp_proxy(tp: &p::TcpProxy, reg: &mut Registry, rbac: Option<Arc<RbacCfg>>) -> R<TcpProxyCfg> {
    let mut clusters = Vec::new();
    match &tp.cluster_specifier {
        Some(p::tcp_proxy::ClusterSpecifier::Cluster(c)) => clusters.push((reg.cluster_slot(c), 1)),
        Some(p::tcp_proxy::ClusterSpecifier::WeightedClusters(w)) => {
            for c in &w.clusters {
                clusters.push((reg.cluster_slot(&c.name), c.weight));
            }
        }
        None => return Err("tcp_proxy without cluster".into()),
    }
    let total_weight = clusters.iter().map(|(_, w)| *w).sum();
    Ok(TcpProxyCfg {
        stat_prefix: tp.stat_prefix.clone(),
        clusters,
        total_weight,
        idle_timeout: dur_nonzero(&tp.idle_timeout).or(Some(Duration::from_secs(3600))),
        rbac,
        access_logs: crate::mesh::accesslog::translate(&tp.access_log),
        tunnel: tp
            .tunneling_config
            .as_ref()
            .filter(|t| !t.use_post)
            .map(|t| Arc::from(t.hostname.as_str())),
    })
}

fn http_cm(
    hcm: &p::HttpConnectionManager,
    reg: &mut Registry,
    direction: Direction,
    tcp_rbac: Option<Arc<RbacCfg>>,
) -> R<HttpCm> {
    let routes = match &hcm.route_specifier {
        Some(p::hcm::RouteSpecifier::Rds(r)) => RouteRef::Rds(reg.route_slot(&r.route_config_name)),
        Some(p::hcm::RouteSpecifier::RouteConfig(rc)) => {
            RouteRef::Inline(Arc::new(route_table(rc, reg)?))
        }
        None => return Err("HCM without routes".into()),
    };
    let mut filters = Vec::new();
    let mut peer_metadata = false;
    let mut istio_alpn = false;
    if let Some(r) = tcp_rbac {
        filters.push(HttpFilterCfg::Rbac(r));
    }
    for f in &hcm.http_filters {
        if f.disabled {
            continue;
        }
        if let Some(cd) = &f.config_discovery {
            // ECDS（Istio の WasmPlugin）。届くまでは既定の設定があればそれを使う
            let slot = reg.ecds_slot(&f.name);
            if slot.filter.load().is_none() {
                if let Some(def) = &cd.default_config {
                    match ecds_filter(&f.name, def) {
                        Ok(fc) => slot.filter.store(Some(Arc::new(fc))),
                        Err(e) => ftlog::warn!("[mesh] ECDS default for {}: {}", f.name, e),
                    }
                }
            }
            filters.push(HttpFilterCfg::Ecds(slot));
            continue;
        }
        let ty = f.typed_config.as_ref().map(inner_type).unwrap_or_default();
        match f.name.as_str() {
            "envoy.filters.http.router" => continue,
            "istio.metadata_exchange" => {
                peer_metadata = true;
                continue;
            }
            "istio.alpn" => {
                istio_alpn = true;
                continue;
            }
            "istio.stats" | "envoy.filters.http.grpc_stats" => continue,
            "envoy.filters.http.cors" => {
                filters.push(HttpFilterCfg::Cors);
                continue;
            }
            "envoy.filters.http.fault" => {
                let fault = match f
                    .typed_config
                    .as_ref()
                    .and_then(|a| decode_any::<p::HttpFault>(a, p::T_FAULT))
                {
                    Some(hf) => fault_cfg(&hf)?,
                    None => FaultCfg {
                        delay: None,
                        abort_http: None,
                        abort_grpc: None,
                        headers: Vec::new(),
                    },
                };
                filters.push(HttpFilterCfg::Fault(Arc::new(fault)));
                continue;
            }
            _ => {}
        }
        if ty == crate::mesh::jwt::T_JWT || f.name == "envoy.filters.http.jwt_authn" {
            let any = f.typed_config.as_ref().ok_or("jwt_authn without config")?;
            filters.push(HttpFilterCfg::Jwt(Arc::new(crate::mesh::jwt::translate(any)?)));
            continue;
        }
        if ty.contains("filters.http.rbac") || f.name == "envoy.filters.http.rbac" {
            let any = f.typed_config.as_ref().ok_or("rbac without config")?;
            filters.push(HttpFilterCfg::Rbac(Arc::new(super::rbac::http_rbac(any)?)));
            continue;
        }
        if ty.contains("peer_metadata") || ty.contains("metadataexchange") {
            // ヘッダでの交換（`istio_headers`）だけを扱う。waypoint の workload_discovery だけの設定は観測用
            peer_metadata |= f
                .typed_config
                .as_ref()
                .is_none_or(|a| !is_typed_struct(a) || typed_struct_json(a).contains("istio_headers"));
            continue;
        }
        if ty.contains("set_filter_state") {
            continue;
        }
        if ty.contains("alpn") {
            istio_alpn = true;
            continue;
        }
        if ty.ends_with("stats.PluginConfig") || ty.contains("grpc_stats") {
            continue;
        }
        if f.is_optional {
            ftlog::debug!("[mesh] ignoring optional HTTP filter {} ({})", f.name, ty);
            continue;
        }
        return Err(format!("unsupported HTTP filter {} ({})", f.name, ty));
    }
    let scd = hcm
        .set_current_client_cert_details
        .clone()
        .unwrap_or_default();
    let trace_sampling = hcm
        .tracing
        .as_ref()
        .and_then(|t| t.random_sampling.as_ref())
        .map(|p| (p.value * 10_000.0) as u32)
        .unwrap_or(0);
    let tracer = hcm
        .tracing
        .as_ref()
        .and_then(|t| crate::mesh::tracing::from_full(t, reg))
        .map(Arc::new);
    Ok(HttpCm {
        stat_prefix: hcm.stat_prefix.clone(),
        routes,
        codec: match hcm.codec_type {
            1 => Codec::Http1,
            2 => Codec::Http2,
            _ => Codec::Auto,
        },
        server_name: Bytes::from(if hcm.server_name.is_empty() {
            "envoy".to_string()
        } else {
            hcm.server_name.clone()
        }),
        xfcc: match hcm.forward_client_cert_details {
            1 => XfccMode::ForwardOnly,
            2 => XfccMode::AppendForward,
            3 => XfccMode::SanitizeSet,
            4 => XfccMode::AlwaysForwardOnly,
            _ => XfccMode::Sanitize,
        },
        xfcc_subject: scd.subject.as_ref().map(|b| b.value).unwrap_or(false),
        xfcc_uri: scd.uri,
        xfcc_dns: scd.dns,
        generate_request_id: hcm
            .generate_request_id
            .as_ref()
            .map(|b| b.value)
            .unwrap_or(true),
        use_remote_address: hcm
            .use_remote_address
            .as_ref()
            .map(|b| b.value)
            .unwrap_or(false),
        websocket: hcm.upgrade_configs.iter().any(|u| {
            u.upgrade_type.eq_ignore_ascii_case("websocket")
                && u.enabled.as_ref().map(|b| b.value).unwrap_or(true)
        }),
        filters,
        peer_metadata,
        istio_alpn,
        direction,
        stream_idle_timeout: dur_nonzero(&hcm.stream_idle_timeout),
        trace_sampling,
        access_logs: crate::mesh::accesslog::translate(&hcm.access_log),
        tracer,
        linkerd: false,
    })
}

fn fault_cfg(f: &p::HttpFault) -> R<FaultCfg> {
    Ok(FaultCfg {
        delay: f.delay.as_ref().and_then(|d| {
            Some((
                d.fixed_delay.as_ref()?.to_std(),
                d.percentage
                    .as_ref()
                    .map(|p| p.per_million())
                    .unwrap_or(1_000_000),
            ))
        }),
        abort_http: f.abort.as_ref().and_then(|a| {
            (a.http_status != 0).then(|| {
                (
                    a.http_status as u16,
                    a.percentage
                        .as_ref()
                        .map(|p| p.per_million())
                        .unwrap_or(1_000_000),
                )
            })
        }),
        abort_grpc: f.abort.as_ref().and_then(|a| {
            (a.http_status == 0 && a.grpc_status != 0).then(|| {
                (
                    a.grpc_status,
                    a.percentage
                        .as_ref()
                        .map(|p| p.per_million())
                        .unwrap_or(1_000_000),
                )
            })
        }),
        headers: f.headers.iter().map(header_match).collect::<R<_>>()?,
    })
}

// ---------------------------------------------------------------------------
// ルート
// ---------------------------------------------------------------------------

pub fn route_table(rc: &p::RouteConfiguration, reg: &mut Registry) -> R<RouteTable> {
    let mut t = RouteTable::empty(&rc.name);
    t.ignore_port = rc.ignore_port_in_host_matching;
    t.request_ops = header_ops(&rc.request_headers_to_add, &rc.request_headers_to_remove);
    t.response_ops = header_ops(&rc.response_headers_to_add, &rc.response_headers_to_remove);
    for vh in &rc.virtual_hosts {
        let idx = t.vhosts.len();
        t.vhosts
            .push(virtual_host(vh, reg).map_err(|e| format!("vhost {}: {}", vh.name, e))?);
        for d in &vh.domains {
            let d = d.to_ascii_lowercase();
            if d == "*" {
                t.catch_all.get_or_insert(idx);
            } else if let Some(suf) = d.strip_prefix('*') {
                t.suffix.push((suf.to_string(), idx));
            } else if let Some(pre) = d.strip_suffix('*') {
                t.prefix.push((pre.to_string(), idx));
            } else {
                t.exact.entry(d).or_insert(idx);
            }
        }
    }
    t.suffix.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    t.prefix.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    Ok(t)
}

fn retry_policy(rp: &p::RetryPolicy) -> Arc<RetryPolicy> {
    let mut r = RetryPolicy {
        num_retries: rp.num_retries.as_ref().map(|n| n.value).unwrap_or(1),
        per_try_timeout: dur_nonzero(&rp.per_try_timeout),
        on_5xx: false,
        on_gateway_error: false,
        on_connect_failure: false,
        on_reset: false,
        on_refused_stream: false,
        on_reset_before_request: false,
        on_retriable_status: false,
        retriable_status_codes: rp
            .retriable_status_codes
            .iter()
            .map(|c| *c as u16)
            .collect(),
        grpc_codes: Vec::new(),
    };
    for tok in rp.retry_on.split(',').map(str::trim) {
        match tok {
            "5xx" => r.on_5xx = true,
            "gateway-error" => r.on_gateway_error = true,
            "connect-failure" => r.on_connect_failure = true,
            "reset" => r.on_reset = true,
            "refused-stream" => r.on_refused_stream = true,
            "reset-before-request" => r.on_reset_before_request = true,
            "retriable-status-codes" => r.on_retriable_status = true,
            "cancelled" => r.grpc_codes.push(1),
            "deadline-exceeded" => r.grpc_codes.push(4),
            "resource-exhausted" => r.grpc_codes.push(8),
            "internal" => r.grpc_codes.push(13),
            "unavailable" => r.grpc_codes.push(14),
            _ => {}
        }
    }
    Arc::new(r)
}

fn per_filter_fault(m: &HashMap<String, p::Any>) -> R<Option<Arc<FaultCfg>>> {
    match m.get("envoy.filters.http.fault") {
        Some(any) => match decode_any::<p::HttpFault>(any, p::T_FAULT) {
            Some(f) => Ok(Some(Arc::new(fault_cfg(&f)?))),
            None => Ok(None),
        },
        None => Ok(None),
    }
}

fn per_filter_cors(
    m: &HashMap<String, p::Any>,
    legacy: Option<&p::CorsPolicy>,
) -> R<Option<Arc<CorsCfg>>> {
    let pol = match m.get("envoy.filters.http.cors") {
        Some(any) => decode_any::<p::CorsPolicy>(
            any,
            "type.googleapis.com/envoy.extensions.filters.http.cors.v3.CorsPolicy",
        ),
        None => legacy.cloned(),
    };
    let Some(c) = pol else { return Ok(None) };
    Ok(Some(Arc::new(CorsCfg {
        allow_origins: c
            .allow_origin_string_match
            .iter()
            .map(string_matcher)
            .collect::<R<_>>()?,
        allow_methods: Bytes::from(c.allow_methods),
        allow_headers: Bytes::from(c.allow_headers),
        expose_headers: Bytes::from(c.expose_headers),
        max_age: Bytes::from(c.max_age),
        allow_credentials: c.allow_credentials.map(|b| b.value).unwrap_or(false),
    })))
}

fn virtual_host(vh: &p::VirtualHost, reg: &mut Registry) -> R<VirtualHost> {
    let mut routes = Vec::with_capacity(vh.routes.len());
    for r in &vh.routes {
        routes.push(route(r, reg).map_err(|e| format!("route {}: {}", r.name, e))?);
    }
    Ok(VirtualHost {
        name: vh.name.clone(),
        routes,
        request_ops: header_ops(&vh.request_headers_to_add, &vh.request_headers_to_remove),
        response_ops: header_ops(&vh.response_headers_to_add, &vh.response_headers_to_remove),
        include_attempt_count: vh.include_request_attempt_count,
        retry: vh.retry_policy.as_ref().map(retry_policy),
        cors: per_filter_cors(&vh.typed_per_filter_config, vh.cors.as_ref())?,
        fault: per_filter_fault(&vh.typed_per_filter_config)?,
    })
}

fn route(r: &p::Route, reg: &mut Registry) -> R<MeshRoute> {
    let m = r.r#match.clone().unwrap_or_default();
    let case_sensitive = m.case_sensitive.as_ref().map(|b| b.value).unwrap_or(true);
    use p::route_match::PathSpecifier as PS;
    let path = match &m.path_specifier {
        Some(PS::Prefix(s)) if s.is_empty() || s == "/" => PathMatch::Any,
        Some(PS::Prefix(s)) => PathMatch::Prefix(s.clone()),
        Some(PS::Path(s)) => PathMatch::Exact(s.clone()),
        Some(PS::SafeRegex(re)) => PathMatch::Regex(compile_regex(&re.regex)?),
        Some(PS::PathSeparatedPrefix(s)) => PathMatch::Separated(s.clone()),
        Some(PS::ConnectMatcher(_)) => PathMatch::Connect,
        None => PathMatch::Any,
    };
    let headers = m.headers.iter().map(header_match).collect::<R<Vec<_>>>()?;
    let query = m
        .query_parameters
        .iter()
        .map(|q| {
            Ok(QueryMatch {
                name: q.name.clone(),
                value: match &q.specifier {
                    Some(p::query_parameter_matcher::Specifier::StringMatch(sm)) => {
                        Some(string_matcher(sm)?)
                    }
                    _ => None,
                },
            })
        })
        .collect::<R<Vec<_>>>()?;
    let action = match &r.action {
        Some(p::route::Action::Route(ra)) => RouteAction::Forward(Arc::new(forward(ra, reg)?)),
        Some(p::route::Action::Redirect(rd)) => RouteAction::Redirect(Arc::new(RedirectCfg {
            host: (!rd.host_redirect.is_empty()).then(|| rd.host_redirect.clone()),
            port: (rd.port_redirect != 0).then_some(rd.port_redirect as u16),
            scheme: if rd.https_redirect {
                Some("https".into())
            } else {
                (!rd.scheme_redirect.is_empty()).then(|| rd.scheme_redirect.clone())
            },
            path: (!rd.path_redirect.is_empty()).then(|| rd.path_redirect.clone()),
            prefix_rewrite: (!rd.prefix_rewrite.is_empty()).then(|| rd.prefix_rewrite.clone()),
            strip_query: rd.strip_query,
            status: match rd.response_code {
                1 => 302,
                2 => 303,
                3 => 307,
                4 => 308,
                _ => 301,
            },
        })),
        Some(p::route::Action::DirectResponse(d)) => RouteAction::Direct {
            status: d.status as u16,
            body: d
                .body
                .as_ref()
                .and_then(|b| b.inline())
                .map(|b| Bytes::copy_from_slice(b))
                .unwrap_or_default(),
        },
        Some(p::route::Action::NonForwardingAction(_)) => RouteAction::NonForwarding,
        None => return Err("route without action".into()),
    };
    let cors_legacy = match &r.action {
        Some(p::route::Action::Route(ra)) => ra.cors.as_ref(),
        _ => None,
    };
    Ok(MeshRoute {
        name: r.name.clone(),
        path,
        case_sensitive,
        headers,
        query,
        fraction: m
            .runtime_fraction
            .as_ref()
            .and_then(|f| f.default_value.as_ref())
            .map(|p| p.per_million()),
        grpc_only: m.grpc.is_some(),
        action,
        request_ops: header_ops(&r.request_headers_to_add, &r.request_headers_to_remove),
        response_ops: header_ops(&r.response_headers_to_add, &r.response_headers_to_remove),
        operation: Bytes::from(
            r.decorator
                .as_ref()
                .map(|d| d.operation.clone())
                .unwrap_or_default(),
        ),
        fault: per_filter_fault(&r.typed_per_filter_config)?,
        cors: per_filter_cors(&r.typed_per_filter_config, cors_legacy)?,
        rbac: None,
    })
}

fn forward(ra: &p::RouteAction, reg: &mut Registry) -> R<ForwardAction> {
    use p::route_action::ClusterSpecifier as CS;
    let target = match &ra.cluster_specifier {
        Some(CS::Cluster(c)) => ClusterTarget::Single(reg.cluster_slot(c)),
        Some(CS::ClusterHeader(h)) => ClusterTarget::Header(h.to_ascii_lowercase()),
        Some(CS::WeightedClusters(w)) => {
            let clusters: Vec<WeightedTarget> = w
                .clusters
                .iter()
                .map(|c| WeightedTarget {
                    cluster: reg.cluster_slot(&c.name),
                    weight: c.weight.as_ref().map(|w| w.value).unwrap_or(0),
                    request_ops: header_ops(
                        &c.request_headers_to_add,
                        &c.request_headers_to_remove,
                    ),
                    response_ops: header_ops(
                        &c.response_headers_to_add,
                        &c.response_headers_to_remove,
                    ),
                })
                .collect();
            let total = w
                .total_weight
                .as_ref()
                .map(|t| t.value)
                .unwrap_or_else(|| clusters.iter().map(|c| c.weight).sum());
            ClusterTarget::Weighted { clusters, total }
        }
        None => return Err("route action without cluster".into()),
    };
    let (host_rewrite, auto_host_rewrite) = match &ra.host_rewrite {
        Some(p::route_action::HostRewrite::HostRewriteLiteral(h)) => {
            (Some(Bytes::from(h.clone())), false)
        }
        Some(p::route_action::HostRewrite::AutoHostRewrite(b)) => (None, b.value),
        None => (None, false),
    };
    let regex_rewrite = match &ra.regex_rewrite {
        Some(rr) => match &rr.pattern {
            Some(pat) => Some((compile_regex(&pat.regex)?, rr.substitution.clone())),
            None => None,
        },
        None => None,
    };
    let hash = ra
        .hash_policy
        .iter()
        .filter_map(|h| match &h.specifier {
            Some(p::hash_policy::Specifier::Header(hh)) => {
                Some(HashPolicy::Header(hh.header_name.to_ascii_lowercase()))
            }
            Some(p::hash_policy::Specifier::Cookie(c)) => Some(HashPolicy::Cookie(c.name.clone())),
            Some(p::hash_policy::Specifier::ConnectionProperties(c)) if c.source_ip => {
                Some(HashPolicy::SourceIp)
            }
            Some(p::hash_policy::Specifier::QueryParameter(q)) => {
                Some(HashPolicy::Query(q.name.clone()))
            }
            _ => None,
        })
        .collect();
    let mirrors = ra
        .request_mirror_policies
        .iter()
        .filter(|m| !m.cluster.is_empty())
        .map(|m| {
            (
                reg.cluster_slot(&m.cluster),
                m.runtime_fraction
                    .as_ref()
                    .and_then(|f| f.default_value.as_ref())
                    .map(|p| p.per_million())
                    .unwrap_or(1_000_000),
                !m.disable_shadow_host_suffix_append,
            )
        })
        .collect();
    let websocket = ra
        .upgrade_configs
        .iter()
        .find(|u| u.upgrade_type.eq_ignore_ascii_case("websocket"))
        .map(|u| u.enabled.as_ref().map(|b| b.value).unwrap_or(true));
    let connect_terminate = ra
        .upgrade_configs
        .iter()
        .any(|u| u.upgrade_type.eq_ignore_ascii_case("CONNECT") && u.connect_config.is_some());
    Ok(ForwardAction {
        connect_terminate,
        target,
        timeout: dur_nonzero(&ra.timeout).or_else(|| {
            // Istio は既定で 0s（無制限）を明示する。未指定の Envoy の既定は 15s
            if ra.timeout.is_some() {
                None
            } else {
                Some(Duration::from_secs(15))
            }
        }),
        idle_timeout: dur_nonzero(&ra.idle_timeout),
        retry: ra.retry_policy.as_ref().map(retry_policy),
        prefix_rewrite: (!ra.prefix_rewrite.is_empty())
            .then(|| Bytes::from(ra.prefix_rewrite.clone())),
        regex_rewrite,
        host_rewrite,
        auto_host_rewrite,
        mirrors,
        hash,
        websocket,
    })
}

// ---------------------------------------------------------------------------
// クラスタ・エンドポイント
// ---------------------------------------------------------------------------

pub fn cluster(c: &p::Cluster) -> R<ClusterCfg> {
    let kind = match c.r#type {
        0 => ClusterKind::Static,
        1 | 2 => ClusterKind::StrictDns,
        3 => ClusterKind::Eds,
        4 => ClusterKind::OriginalDst,
        t => return Err(format!("cluster {}: unsupported type {}", c.name, t)),
    };
    let lb = match c.lb_policy {
        1 => LbPolicy::LeastRequest,
        2 => LbPolicy::RingHash,
        3 => LbPolicy::Random,
        5 => LbPolicy::Maglev,
        _ => LbPolicy::RoundRobin,
    };
    let mut tls_matches = Vec::new();
    for m in &c.transport_socket_matches {
        let crit = m.r#match.as_ref().map(struct_strings).unwrap_or_default();
        let tls = match &m.transport_socket {
            Some(ts) => {
                transport_socket_upstream(ts).map_err(|e| format!("cluster {}: {}", c.name, e))?
            }
            None => None,
        };
        tls_matches.push((crit, tls));
    }
    let tls_default = match &c.transport_socket {
        Some(ts) => {
            transport_socket_upstream(ts).map_err(|e| format!("cluster {}: {}", c.name, e))?
        }
        None => None,
    };
    let mut proto = UpstreamProto::Http1;
    let mut idle_timeout = Duration::from_secs(3600);
    let mut max_requests_per_conn = 0;
    if let Some(any) = c
        .typed_extension_protocol_options
        .get("envoy.extensions.upstreams.http.v3.HttpProtocolOptions")
    {
        if let Some(hpo) = decode_any::<p::HttpProtocolOptions>(any, p::T_HTTP_PROTOCOL_OPTIONS) {
            proto = match &hpo.upstream_protocol_options {
                Some(p::http_protocol_options::UpstreamProtocolOptions::ExplicitHttpConfig(e)) => {
                    if e.http2_protocol_options.is_some() {
                        UpstreamProto::Http2
                    } else {
                        UpstreamProto::Http1
                    }
                }
                Some(
                    p::http_protocol_options::UpstreamProtocolOptions::UseDownstreamProtocolConfig(
                        _,
                    ),
                ) => UpstreamProto::UseDownstream,
                Some(p::http_protocol_options::UpstreamProtocolOptions::AutoConfig(_)) => {
                    UpstreamProto::UseDownstream
                }
                None => UpstreamProto::Http1,
            };
            if let Some(common) = &hpo.common_http_protocol_options {
                if let Some(d) = dur_nonzero(&common.idle_timeout) {
                    idle_timeout = d;
                }
                if let Some(m) = &common.max_requests_per_connection {
                    max_requests_per_conn = m.value;
                }
            }
        }
    }
    let th = c
        .circuit_breakers
        .as_ref()
        .and_then(|cb| cb.thresholds.iter().find(|t| t.priority == 0).cloned())
        .unwrap_or_default();
    let lim = |v: &Option<p::UInt32Value>, d: u32| v.as_ref().map(|x| x.value).unwrap_or(d);
    let outlier = c.outlier_detection.as_ref().map(|o| OutlierCfg {
        consecutive_5xx: o.consecutive_5xx.as_ref().map(|x| x.value).unwrap_or(5),
        consecutive_gateway: o
            .consecutive_gateway_failure
            .as_ref()
            .map(|x| x.value)
            .unwrap_or(0),
        base_ejection: dur(&o.base_ejection_time).unwrap_or(Duration::from_secs(30)),
        max_ejection_percent: o
            .max_ejection_percent
            .as_ref()
            .map(|x| x.value)
            .unwrap_or(10),
        interval: dur(&o.interval).unwrap_or(Duration::from_secs(10)),
    });
    let (mut service_host, mut service_name, mut service_namespace) =
        (Bytes::new(), Bytes::new(), Bytes::new());
    if let Some(md) = &c.metadata {
        if let Some(istio) = md.filter_metadata.get("istio") {
            if let Some(svc) = istio
                .fields
                .get("services")
                .and_then(|v| v.as_list())
                .and_then(|l| l.values.first())
                .and_then(|v| v.as_struct())
            {
                service_host = Bytes::from(svc.get_str("host").unwrap_or("").to_string());
                service_name = Bytes::from(svc.get_str("name").unwrap_or("").to_string());
                service_namespace = Bytes::from(svc.get_str("namespace").unwrap_or("").to_string());
            }
        }
    }
    let bind = c
        .upstream_bind_config
        .as_ref()
        .and_then(|b| b.source_address.as_ref())
        .and_then(|sa| {
            let ip: std::net::IpAddr = sa.address.parse().ok()?;
            Some(SocketAddr::new(ip, sa.port_value as u16))
        });
    let eds_name = c
        .eds_cluster_config
        .as_ref()
        .map(|e| e.service_name.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| c.name.clone());
    let svc_labels =
        crate::mesh::stats::service_labels(&service_host, &service_name, &service_namespace);
    let port_override = c
        .original_dst_lb_config
        .as_ref()
        .and_then(|o| o.upstream_port_override.as_ref())
        .map(|p| p.value as u16);
    Ok(ClusterCfg {
        port_override,
        svc_labels,
        name: c.name.clone(),
        kind,
        lb,
        connect_timeout: dur(&c.connect_timeout).unwrap_or(Duration::from_secs(5)),
        bind,
        tls_matches,
        tls_default,
        proto,
        idle_timeout,
        max_requests_per_conn,
        outlier,
        max_connections: lim(&th.max_connections, 1024),
        max_pending: lim(&th.max_pending_requests, 1024),
        max_requests: lim(&th.max_requests, 1024),
        max_retries: lim(&th.max_retries, 3),
        service_host,
        service_name,
        service_namespace,
        eds_name,
    })
}

/// 静的クラスタ（`load_assignment`）・STRICT_DNS のエンドポイント。名前の解決はこのスレッドで行う
/// （制御プレーン用の専用スレッド）。
pub fn static_endpoints(c: &p::Cluster) -> Option<EndpointSet> {
    let la = c.load_assignment.as_ref()?;
    Some(endpoint_set(la, c.r#type == 1 || c.r#type == 2))
}

pub fn endpoint_set(la: &p::ClusterLoadAssignment, resolve_names: bool) -> EndpointSet {
    let mut eps = Vec::new();
    for loc in &la.endpoints {
        let locality = loc
            .locality
            .as_ref()
            .map(|l| (l.region.clone(), l.zone.clone(), l.sub_zone.clone()))
            .unwrap_or_default();
        for lb in &loc.lb_endpoints {
            let Some(ep) = &lb.endpoint else { continue };
            let Some(addr_msg) = &ep.address else {
                continue;
            };
            // 内部アドレス: 元の宛先はメタデータ `envoy.filters.listener.original_dst.local`
            let internal = match &addr_msg.address {
                Some(p::address::Address::EnvoyInternalAddress(ia)) => {
                    Some(Arc::<str>::from(ia.server_listener_name.as_str()))
                }
                _ => None,
            };
            let addrs: Vec<SocketAddr> = match addr_msg.socket_addr() {
                None if internal.is_some() => vec![lb
                    .metadata
                    .as_ref()
                    .and_then(|m| m.filter_metadata.get("envoy.filters.listener.original_dst"))
                    .and_then(|s| s.get_str("local"))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)))],
                Some(a) => vec![a],
                None if resolve_names => match &addr_msg.address {
                    Some(p::address::Address::SocketAddress(sa)) => {
                        resolve(&sa.address, sa.port_value as u16)
                    }
                    _ => Vec::new(),
                },
                None => Vec::new(),
            };
            for addr in addrs {
                let mut me = MeshEndpoint::new(addr);
                me.weight = lb
                    .load_balancing_weight
                    .as_ref()
                    .map(|w| w.value)
                    .unwrap_or(1)
                    .max(1);
                me.healthy = matches!(lb.health_status, 0 | 1 | 5);
                me.priority = loc.priority;
                me.locality = locality.clone();
                me.internal = internal.clone();
                if let Some(md) = &lb.metadata {
                    if let Some(m) = md.filter_metadata.get("envoy.transport_socket_match") {
                        me.socket_match = struct_strings(m);
                    }
                    if let Some(w) = md
                        .filter_metadata
                        .get("istio")
                        .and_then(|s| s.get_str("workload"))
                    {
                        me.workload = Bytes::from(w.to_string());
                    }
                }
                eps.push(me);
            }
        }
    }
    EndpointSet::new(eps)
}

// 理由付き allow: xDS の適用スレッド（制御プレーン用の専用スレッド）でだけ呼ぶ。
#[allow(clippy::disallowed_methods)]
fn resolve(host: &str, port: u16) -> Vec<SocketAddr> {
    use std::net::ToSocketAddrs;
    (host, port)
        .to_socket_addrs()
        .map(|it| it.collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::state::Registry;

    /// waypoint の `main_internal` と同じ形の `filter_chain_matcher`（宛先 IP → 宛先ポート → チェーン名）。
    #[test]
    fn filter_chain_matcher_selects_by_destination_ip_and_port() {
        use prost::Message;
        let tec = |name: &str, ty: &str, value: Vec<u8>| p::TypedExtensionConfig {
            name: name.into(),
            typed_config: Some(p::Any {
                type_url: format!("type.googleapis.com/{}", ty),
                value,
            }),
        };
        let action = |chain: &str| p::XdsOnMatch {
            on_match: Some(p::xds_matcher::OnMatch::Action(tec(
                chain,
                "google.protobuf.StringValue",
                p::StringValue {
                    value: chain.into(),
                }
                .encode_to_vec(),
            ))),
            keep_matching: false,
        };
        let by_port = p::XdsMatcher {
            matcher_type: Some(p::xds_matcher::MatcherType::MatcherTree(p::XdsMatcherTree {
                input: Some(tec(
                    "port",
                    "envoy.extensions.matching.common_inputs.network.v3.DestinationPortInput",
                    Vec::new(),
                )),
                tree_type: Some(p::xds_matcher::TreeType::ExactMatchMap(p::XdsMatchMap {
                    map: [("80".to_string(), action("svc-80"))].into_iter().collect(),
                })),
            })),
            on_no_match: None,
        };
        let ipm = p::XdsIpMatcher {
            range_matchers: vec![p::XdsIpRangeMatcher {
                ranges: vec![p::CidrRange {
                    address_prefix: "10.96.0.10".into(),
                    prefix_len: Some(p::UInt32Value { value: 32 }),
                }],
                on_match: Some(p::XdsOnMatch {
                    on_match: Some(p::xds_matcher::OnMatch::Matcher(Box::new(by_port))),
                    keep_matching: false,
                }),
                exclusive: false,
            }],
        };
        let m = p::XdsMatcher {
            matcher_type: Some(p::xds_matcher::MatcherType::MatcherTree(p::XdsMatcherTree {
                input: Some(tec(
                    "ip",
                    "envoy.extensions.matching.common_inputs.network.v3.DestinationIPInput",
                    Vec::new(),
                )),
                tree_type: Some(p::xds_matcher::TreeType::CustomMatch(tec(
                    "ip",
                    "xds.type.matcher.v3.IPMatcher",
                    ipm.encode_to_vec(),
                ))),
            })),
            on_no_match: Some(Box::new(action("direct"))),
        };
        let chains = vec![
            p::FilterChain {
                name: "svc-80".into(),
                ..Default::default()
            },
            p::FilterChain {
                name: "direct".into(),
                ..Default::default()
            },
        ];
        // ワイヤ形式を経由して変換する
        let m = p::XdsMatcher::decode(m.encode_to_vec().as_slice()).unwrap();
        let cm = chain_matcher(&m, &chains).unwrap();
        let facts = |dst: &str| ConnFacts {
            dst: Some(dst.parse().unwrap()),
            src: None,
            sni: None,
            transport: None,
            alpn: &[],
        };
        assert_eq!(cm.eval(&facts("10.96.0.10:80")), Some(0));
        assert_eq!(cm.eval(&facts("10.96.0.10:81")), Some(1));
        assert_eq!(cm.eval(&facts("10.96.0.11:80")), Some(1));
    }

    #[test]
    fn route_table_selects_vhost_like_envoy() {
        let rc = p::RouteConfiguration {
            name: "80".into(),
            virtual_hosts: vec![
                p::VirtualHost {
                    name: "whoami".into(),
                    domains: vec![
                        "whoami.demo.svc.cluster.local".into(),
                        "whoami".into(),
                        "10.96.0.5".into(),
                    ],
                    routes: vec![p::Route {
                        name: "default".into(),
                        r#match: Some(p::RouteMatch {
                            path_specifier: Some(p::route_match::PathSpecifier::Prefix("/".into())),
                            ..Default::default()
                        }),
                        action: Some(p::route::Action::Route(p::RouteAction {
                            cluster_specifier: Some(p::route_action::ClusterSpecifier::Cluster(
                                "outbound|80||whoami.demo.svc.cluster.local".into(),
                            )),
                            timeout: Some(p::Duration {
                                seconds: 0,
                                nanos: 0,
                            }),
                            ..Default::default()
                        })),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                p::VirtualHost {
                    name: "wild".into(),
                    domains: vec!["*.example.com".into()],
                    ..Default::default()
                },
                p::VirtualHost {
                    name: "allow_any".into(),
                    domains: vec!["*".into()],
                    ..Default::default()
                },
            ],
            ignore_port_in_host_matching: true,
            ..Default::default()
        };
        let mut reg = Registry::default();
        let t = route_table(&rc, &mut reg).unwrap();
        assert_eq!(t.select_vhost(b"whoami").unwrap().name, "whoami");
        assert_eq!(t.select_vhost(b"WHOAMI:80").unwrap().name, "whoami");
        assert_eq!(t.select_vhost(b"10.96.0.5:80").unwrap().name, "whoami");
        assert_eq!(t.select_vhost(b"a.example.com").unwrap().name, "wild");
        assert_eq!(t.select_vhost(b"other").unwrap().name, "allow_any");
        let r = &t.vhosts[0].routes[0];
        match &r.action {
            RouteAction::Forward(f) => {
                assert!(f.timeout.is_none(), "Istio の 0s は無制限");
                assert!(
                    reg.clusters
                        .contains_key("outbound|80||whoami.demo.svc.cluster.local")
                );
            }
            _ => panic!(),
        }
    }
}
