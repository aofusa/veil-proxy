//! Envoy の RBAC フィルタ（Istio の AuthorizationPolicy）の変換（F-220）。
//!
//! 変換できない規則（CEL の条件・未知のマッチャ等）は安全側に倒す: ALLOW のポリシーなら
//! そのポリシーを一致しない（＝許さない）扱いに、DENY のポリシーなら一致する（＝拒否する）扱いにする。

use super::proto::{self as p, decode_any, Any};
use super::translate::{string_matcher, R};
use crate::mesh::model::*;

// ---------------------------------------------------------------------------
// proto（envoy.config.rbac.v3 と envoy.extensions.filters.{http,network}.rbac.v3）
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub struct RbacFilterHttp {
    #[prost(message, optional, tag = "1")]
    pub rules: Option<Rbac>,
    #[prost(message, optional, tag = "2")]
    pub shadow_rules: Option<Rbac>,
    #[prost(message, optional, tag = "4")]
    pub matcher: Option<p::Empty>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RbacFilterNetwork {
    #[prost(message, optional, tag = "1")]
    pub rules: Option<Rbac>,
    #[prost(message, optional, tag = "2")]
    pub shadow_rules: Option<Rbac>,
    #[prost(message, optional, tag = "6")]
    pub matcher: Option<p::Empty>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Rbac {
    /// 0 = ALLOW, 1 = DENY, 2 = LOG
    #[prost(int32, tag = "1")]
    pub action: i32,
    #[prost(map = "string, message", tag = "2")]
    pub policies: std::collections::HashMap<String, Policy>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Policy {
    #[prost(message, repeated, tag = "1")]
    pub permissions: Vec<Permission>,
    #[prost(message, repeated, tag = "2")]
    pub principals: Vec<Principal>,
    /// CEL（対応しない。あれば安全側に倒す）
    #[prost(message, optional, tag = "3")]
    pub condition: Option<p::Empty>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Permission {
    #[prost(
        oneof = "permission::Rule",
        tags = "1, 2, 3, 4, 10, 5, 6, 11, 7, 8, 9, 12"
    )]
    pub rule: Option<permission::Rule>,
}

pub mod permission {
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Set {
        #[prost(message, repeated, tag = "1")]
        pub rules: Vec<super::Permission>,
    }
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Rule {
        #[prost(message, tag = "1")]
        AndRules(Set),
        #[prost(message, tag = "2")]
        OrRules(Set),
        #[prost(bool, tag = "3")]
        Any(bool),
        #[prost(message, tag = "4")]
        Header(super::p::HeaderMatcher),
        #[prost(message, tag = "10")]
        UrlPath(super::PathMatcher),
        #[prost(message, tag = "5")]
        DestinationIp(super::p::CidrRange),
        #[prost(uint32, tag = "6")]
        DestinationPort(u32),
        #[prost(message, tag = "11")]
        DestinationPortRange(super::Int32Range),
        #[prost(message, tag = "7")]
        Metadata(super::MetadataMatcher),
        #[prost(message, tag = "8")]
        NotRule(Box<super::Permission>),
        #[prost(message, tag = "9")]
        RequestedServerName(super::p::StringMatcher),
        #[prost(message, tag = "12")]
        Matcher(super::p::Empty),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Principal {
    #[prost(
        oneof = "principal::Identifier",
        tags = "1, 2, 3, 4, 5, 10, 11, 6, 9, 7, 8, 12"
    )]
    pub identifier: Option<principal::Identifier>,
}

pub mod principal {
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Set {
        #[prost(message, repeated, tag = "1")]
        pub ids: Vec<super::Principal>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Authenticated {
        #[prost(message, optional, tag = "2")]
        pub principal_name: Option<super::p::StringMatcher>,
    }
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Identifier {
        #[prost(message, tag = "1")]
        AndIds(Set),
        #[prost(message, tag = "2")]
        OrIds(Set),
        #[prost(bool, tag = "3")]
        Any(bool),
        #[prost(message, tag = "4")]
        Authenticated(Authenticated),
        #[prost(message, tag = "5")]
        SourceIp(super::p::CidrRange),
        #[prost(message, tag = "10")]
        DirectRemoteIp(super::p::CidrRange),
        #[prost(message, tag = "11")]
        RemoteIp(super::p::CidrRange),
        #[prost(message, tag = "6")]
        Header(super::p::HeaderMatcher),
        #[prost(message, tag = "9")]
        UrlPath(super::PathMatcher),
        #[prost(message, tag = "7")]
        Metadata(super::MetadataMatcher),
        #[prost(message, tag = "8")]
        NotId(Box<super::Principal>),
        /// filter state（ambient の waypoint は相手の ID を `io.istio.peer_principal` で見る）
        #[prost(message, tag = "12")]
        FilterState(super::FilterStateMatcher),
    }
}

/// `envoy.type.matcher.v3.FilterStateMatcher`（文字列の照合だけ）
#[derive(Clone, PartialEq, prost::Message)]
pub struct FilterStateMatcher {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(message, optional, tag = "2")]
    pub string_match: Option<p::StringMatcher>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PathMatcher {
    #[prost(message, optional, tag = "1")]
    pub path: Option<p::StringMatcher>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Int32Range {
    #[prost(int32, tag = "1")]
    pub start: i32,
    #[prost(int32, tag = "2")]
    pub end: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct MetadataMatcher {
    #[prost(string, tag = "1")]
    pub filter: String,
    #[prost(message, repeated, tag = "2")]
    pub path: Vec<PathSegment>,
    #[prost(message, optional, tag = "3")]
    pub value: Option<ValueMatcher>,
    #[prost(bool, tag = "4")]
    pub invert: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PathSegment {
    #[prost(string, tag = "1")]
    pub key: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ValueMatcher {
    #[prost(oneof = "value_matcher::Pattern", tags = "3, 4, 5, 6, 7")]
    pub pattern: Option<value_matcher::Pattern>,
}

pub mod value_matcher {
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ListMatcher {
        #[prost(message, optional, boxed, tag = "1")]
        pub one_of: Option<Box<super::ValueMatcher>>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct OrMatcher {
        #[prost(message, repeated, tag = "1")]
        pub value_matchers: Vec<super::ValueMatcher>,
    }
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Pattern {
        #[prost(message, tag = "3")]
        StringMatch(super::p::StringMatcher),
        #[prost(bool, tag = "4")]
        BoolMatch(bool),
        #[prost(bool, tag = "5")]
        PresentMatch(bool),
        #[prost(message, tag = "6")]
        ListMatch(ListMatcher),
        #[prost(message, tag = "7")]
        OrMatch(OrMatcher),
    }
}

// ---------------------------------------------------------------------------
// 変換
// ---------------------------------------------------------------------------

const T_HTTP_RBAC: &str = "type.googleapis.com/envoy.extensions.filters.http.rbac.v3.RBAC";
const T_NET_RBAC: &str = "type.googleapis.com/envoy.extensions.filters.network.rbac.v3.RBAC";

pub fn http_rbac(any: &Any) -> R<RbacCfg> {
    let f: RbacFilterHttp = decode_any(any, T_HTTP_RBAC).ok_or("invalid RBAC (http)")?;
    if f.matcher.is_some() {
        return Err("RBAC matcher API is not supported".into());
    }
    from_rules(f.rules.as_ref(), f.shadow_rules.as_ref())
}

pub fn network_rbac(any: &Any) -> R<RbacCfg> {
    let f: RbacFilterNetwork = decode_any(any, T_NET_RBAC).ok_or("invalid RBAC (network)")?;
    if f.matcher.is_some() {
        return Err("RBAC matcher API is not supported".into());
    }
    from_rules(f.rules.as_ref(), f.shadow_rules.as_ref())
}

fn from_rules(rules: Option<&Rbac>, shadow: Option<&Rbac>) -> R<RbacCfg> {
    // rules が無く shadow だけなら dry-run（判定だけ）
    let (r, is_shadow) = match (rules, shadow) {
        (Some(r), _) => (r, false),
        (None, Some(s)) => (s, true),
        (None, None) => {
            return Ok(RbacCfg {
                action: RbacAction::Allow,
                policies: Vec::new(),
                shadow: true,
            });
        }
    };
    let action = match r.action {
        1 => RbacAction::Deny,
        2 => RbacAction::Log,
        _ => RbacAction::Allow,
    };
    let mut names: Vec<&String> = r.policies.keys().collect();
    names.sort();
    let mut policies = Vec::new();
    for name in names {
        let pol = &r.policies[name];
        let unsupported_condition = pol.condition.is_some();
        // 評価できない葉の値: ALLOW（と LOG）は一致させず、DENY は一致させる（否定の下では反転）
        let mut cx = Cx {
            fallback: matches!(action, RbacAction::Deny),
            unsupported: 0,
        };
        let mut permissions = pol
            .permissions
            .iter()
            .map(|x| perm(x, &mut cx))
            .collect::<R<Vec<_>>>()?;
        let principals = pol
            .principals
            .iter()
            .map(|x| princ(x, &mut cx))
            .collect::<R<Vec<_>>>()?;
        if cx.unsupported > 0 {
            // 設定の受信時だけ（要求ごとではない）
            ftlog::warn!(
                "RBAC policy '{}': {} unsupported rule(s) evaluated as {} (fail-closed)",
                name,
                cx.unsupported,
                if cx.fallback {
                    "matching"
                } else {
                    "not matching"
                }
            );
        }
        if unsupported_condition {
            // CEL は評価できない: ALLOW は一致させない、DENY は常に一致させる
            permissions = match action {
                RbacAction::Deny => vec![RbacPermission::Any],
                _ => vec![RbacPermission::Not(Box::new(RbacPermission::Any))],
            };
        }
        policies.push((
            name.clone(),
            RbacPolicy {
                permissions,
                principals,
            },
        ));
    }
    Ok(RbacCfg {
        action,
        policies,
        shadow: is_shadow,
    })
}

fn header(h: &p::HeaderMatcher) -> R<HeaderMatch> {
    super::translate::header_match(h)
}

fn cidr(c: &p::CidrRange) -> R<Cidr> {
    let addr = c
        .address_prefix
        .parse()
        .map_err(|_| format!("invalid CIDR '{}'", c.address_prefix))?;
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

/// 変換の文脈。
struct Cx {
    /// 評価できない葉を置き換える値（否定の下に入るたびに反転する）
    fallback: bool,
    /// 評価できずに置き換えた葉の数
    unsupported: usize,
}

impl Cx {
    fn unsupported_perm(&mut self) -> RbacPermission {
        self.unsupported += 1;
        if self.fallback {
            RbacPermission::Any
        } else {
            RbacPermission::Not(Box::new(RbacPermission::Any))
        }
    }

    fn unsupported_princ(&mut self) -> RbacPrincipal {
        self.unsupported += 1;
        if self.fallback {
            RbacPrincipal::Any
        } else {
            RbacPrincipal::Not(Box::new(RbacPrincipal::Any))
        }
    }

    /// 否定の中を変換する（安全側の値が反転する）
    fn negated<T>(&mut self, f: impl FnOnce(&mut Self) -> R<T>) -> R<T> {
        self.fallback = !self.fallback;
        let r = f(self);
        self.fallback = !self.fallback;
        r
    }
}

/// `PathMatcher` の照合（空なら `None` = 評価できない）。
fn path_matcher(pm: &PathMatcher) -> R<Option<StringMatcher>> {
    pm.path.as_ref().map(string_matcher).transpose()
}

fn perm(x: &Permission, cx: &mut Cx) -> R<RbacPermission> {
    use permission::Rule as P;
    Ok(match &x.rule {
        Some(P::AndRules(s)) => {
            RbacPermission::And(s.rules.iter().map(|r| perm(r, cx)).collect::<R<_>>()?)
        }
        Some(P::OrRules(s)) => {
            RbacPermission::Or(s.rules.iter().map(|r| perm(r, cx)).collect::<R<_>>()?)
        }
        Some(P::Any(true)) => RbacPermission::Any,
        Some(P::Any(false)) => RbacPermission::Not(Box::new(RbacPermission::Any)),
        Some(P::Header(h)) => RbacPermission::Header(header(h)?),
        Some(P::UrlPath(pm)) => match path_matcher(pm)? {
            Some(m) => RbacPermission::Path(m),
            None => cx.unsupported_perm(),
        },
        Some(P::DestinationIp(c)) => RbacPermission::DestIp(cidr(c)?),
        Some(P::DestinationPort(port)) => RbacPermission::DestPort(*port as u16),
        Some(P::DestinationPortRange(r)) => RbacPermission::Or(
            (r.start.max(0)..r.end.max(0))
                .take(65536)
                .map(|p| RbacPermission::DestPort(p as u16))
                .collect(),
        ),
        Some(P::NotRule(n)) => RbacPermission::Not(Box::new(cx.negated(|cx| perm(n, cx))?)),
        Some(P::RequestedServerName(sm)) => RbacPermission::Sni(string_matcher(sm)?),
        // 評価できない: ポリシーの種類で安全側に倒す（ALLOW は一致させず、DENY は一致させる）
        Some(P::Metadata(_)) | Some(P::Matcher(_)) | None => cx.unsupported_perm(),
    })
}

fn princ(x: &Principal, cx: &mut Cx) -> R<RbacPrincipal> {
    use principal::Identifier as I;
    Ok(match &x.identifier {
        Some(I::AndIds(s)) => {
            RbacPrincipal::And(s.ids.iter().map(|i| princ(i, cx)).collect::<R<_>>()?)
        }
        Some(I::OrIds(s)) => {
            RbacPrincipal::Or(s.ids.iter().map(|i| princ(i, cx)).collect::<R<_>>()?)
        }
        Some(I::Any(true)) => RbacPrincipal::Any,
        Some(I::Any(false)) => RbacPrincipal::Not(Box::new(RbacPrincipal::Any)),
        Some(I::Authenticated(a)) => RbacPrincipal::Authenticated(match &a.principal_name {
            Some(sm) => Some(string_matcher(sm)?),
            None => None,
        }),
        Some(I::SourceIp(c)) => RbacPrincipal::SourceIp(cidr(c)?),
        Some(I::DirectRemoteIp(c)) => RbacPrincipal::DirectRemoteIp(cidr(c)?),
        Some(I::RemoteIp(c)) => RbacPrincipal::RemoteIp(cidr(c)?),
        Some(I::Header(h)) => RbacPrincipal::Header(header(h)?),
        // Envoy の principal の url_path は permission の url_path と同じ照合
        Some(I::UrlPath(pm)) => match path_matcher(pm)? {
            Some(m) => RbacPrincipal::Path(m),
            None => cx.unsupported_princ(),
        },
        Some(I::Metadata(m)) => {
            let pr = RbacPrincipal::Metadata {
                filter: m.filter.clone(),
                path: m.path.iter().map(|s| s.key.clone()).collect(),
                value: match &m.value {
                    Some(v) => meta_match(v)?,
                    None => MetaMatch::Present(true),
                },
            };
            if m.invert {
                RbacPrincipal::Not(Box::new(pr))
            } else {
                pr
            }
        }
        Some(I::NotId(n)) => RbacPrincipal::Not(Box::new(cx.negated(|cx| princ(n, cx))?)),
        // waypoint: HBONE の mTLS の相手（`%DOWNSTREAM_PEER_URI_SAN%`）。veil は内部リスナーへ相手の ID を
        // そのまま引き継ぐので、認証済みの主体と同じに扱う
        Some(I::FilterState(f)) if f.key == "io.istio.peer_principal" => {
            RbacPrincipal::Authenticated(match &f.string_match {
                Some(sm) => Some(string_matcher(sm)?),
                None => None,
            })
        }
        Some(I::FilterState(f)) => {
            return Err(format!("unsupported RBAC filter_state principal {}", f.key));
        }
        None => cx.unsupported_princ(),
    })
}

fn meta_match(v: &ValueMatcher) -> R<MetaMatch> {
    use value_matcher::Pattern as V;
    Ok(match &v.pattern {
        Some(V::StringMatch(sm)) => MetaMatch::Str(string_matcher(sm)?),
        Some(V::BoolMatch(b)) => MetaMatch::Bool(*b),
        Some(V::PresentMatch(p)) => MetaMatch::Present(*p),
        Some(V::ListMatch(l)) => match &l.one_of {
            Some(inner) => MetaMatch::List(Box::new(meta_match(inner)?)),
            None => MetaMatch::Never,
        },
        Some(V::OrMatch(o)) => {
            MetaMatch::Or(o.value_matchers.iter().map(meta_match).collect::<R<_>>()?)
        }
        None => MetaMatch::Never,
    })
}

// ---------------------------------------------------------------------------
// 評価
// ---------------------------------------------------------------------------

/// 評価に使う要求・接続の属性。
pub struct RbacInput<'a> {
    pub header: &'a dyn Fn(&str) -> Option<bytes::Bytes>,
    pub path: &'a [u8],
    pub dst: Option<std::net::SocketAddr>,
    pub sni: Option<&'a str>,
    /// mTLS の相手の ID（URI SAN）
    pub principal: Option<&'a str>,
    pub direct_remote: Option<std::net::IpAddr>,
    pub remote: Option<std::net::IpAddr>,
    /// フィルタのメタデータ（フィルタ名, パス）→ 値（JWT の payload 等）
    pub metadata: &'a dyn Fn(&str, &[String]) -> Option<serde_json::Value>,
    /// HTTP の属性を持つか（TCP の RBAC はヘッダ・パスの条件に一致しない）
    pub http: bool,
}

/// 要求のパス（クエリを除く）の照合。TCP の RBAC では一致しない。
fn path_matches(m: &StringMatcher, i: &RbacInput<'_>) -> bool {
    i.http && {
        let p = i.path;
        let p = p.split(|&b| b == b'?').next().unwrap_or(p);
        m.matches(p)
    }
}

fn eval_perm(x: &RbacPermission, i: &RbacInput<'_>) -> bool {
    match x {
        RbacPermission::Any => true,
        RbacPermission::And(v) => v.iter().all(|p| eval_perm(p, i)),
        RbacPermission::Or(v) => v.iter().any(|p| eval_perm(p, i)),
        RbacPermission::Not(p) => !eval_perm(p, i),
        RbacPermission::Header(h) => i.http && h.matches((i.header)(&h.name).as_deref()),
        RbacPermission::Path(m) => path_matches(m, i),
        RbacPermission::DestIp(c) => i.dst.is_some_and(|d| c.contains(d.ip())),
        RbacPermission::DestPort(port) => i.dst.is_some_and(|d| d.port() == *port),
        RbacPermission::Sni(m) => i.sni.is_some_and(|s| m.matches(s.as_bytes())),
    }
}

fn eval_princ(x: &RbacPrincipal, i: &RbacInput<'_>) -> bool {
    match x {
        RbacPrincipal::Any => true,
        RbacPrincipal::And(v) => v.iter().all(|p| eval_princ(p, i)),
        RbacPrincipal::Or(v) => v.iter().any(|p| eval_princ(p, i)),
        RbacPrincipal::Not(p) => !eval_princ(p, i),
        RbacPrincipal::Authenticated(m) => match (i.principal, m) {
            (Some(_), None) => true,
            (Some(pr), Some(m)) => {
                // Istio は "spiffe://" を外した値でも照合する（cluster.local/ns/x/sa/y）
                m.matches(pr.as_bytes())
                    || pr
                        .strip_prefix("spiffe://")
                        .is_some_and(|s| m.matches(s.as_bytes()))
            }
            (None, _) => false,
        },
        RbacPrincipal::SourceIp(c) | RbacPrincipal::DirectRemoteIp(c) => {
            i.direct_remote.is_some_and(|ip| c.contains(ip))
        }
        RbacPrincipal::RemoteIp(c) => i
            .remote
            .or(i.direct_remote)
            .is_some_and(|ip| c.contains(ip)),
        RbacPrincipal::Header(h) => i.http && h.matches((i.header)(&h.name).as_deref()),
        RbacPrincipal::Path(m) => path_matches(m, i),
        RbacPrincipal::Metadata {
            filter,
            path,
            value,
        } => value.matches((i.metadata)(filter, path).as_ref()),
    }
}

/// ポリシーの名前（一致したもの）。
pub fn matched_policy<'c>(cfg: &'c RbacCfg, i: &RbacInput<'_>) -> Option<&'c str> {
    cfg.policies
        .iter()
        .find(|(_, pol)| {
            pol.permissions.iter().any(|p| eval_perm(p, i))
                && pol.principals.iter().any(|p| eval_princ(p, i))
        })
        .map(|(n, _)| n.as_str())
}

/// 許可するか（shadow は常に許可。判定はログだけ）。
pub fn allowed(cfg: &RbacCfg, i: &RbacInput<'_>) -> bool {
    let m = matched_policy(cfg, i);
    let allow = match cfg.action {
        RbacAction::Allow => m.is_some(),
        RbacAction::Deny => m.is_none(),
        RbacAction::Log => true,
    };
    if cfg.shadow {
        if !allow {
            ftlog::debug!("[mesh] RBAC (dry-run) would deny (policy {:?})", m);
        }
        return true;
    }
    allow
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(
        principal: Option<&'a str>,
        path: &'a [u8],
        hdr: &'a dyn Fn(&str) -> Option<bytes::Bytes>,
        md: &'a dyn Fn(&str, &[String]) -> Option<serde_json::Value>,
    ) -> RbacInput<'a> {
        RbacInput {
            header: hdr,
            path,
            dst: Some("10.0.0.1:8080".parse().unwrap()),
            sni: None,
            principal,
            direct_remote: Some("10.0.0.2".parse().unwrap()),
            remote: None,
            metadata: md,
            http: true,
        }
    }

    /// ambient の waypoint の認可: 相手の ID は filter state（`io.istio.peer_principal`）で来る（F-224）。
    #[test]
    fn waypoint_filter_state_principal_is_decoded_and_matched() {
        use prost::Message;
        let pr = Principal {
            identifier: Some(principal::Identifier::FilterState(FilterStateMatcher {
                key: "io.istio.peer_principal".into(),
                string_match: Some(p::StringMatcher {
                    ignore_case: false,
                    pattern: Some(p::string_matcher::Pattern::Exact(
                        "spiffe://cluster.local/ns/demo/sa/client".into(),
                    )),
                }),
            })),
        };
        // ワイヤ形式を経由しても失われない（oneof の tags に 12 が無いと None になり全拒否になっていた）
        let decoded = Principal::decode(pr.encode_to_vec().as_slice()).unwrap();
        let mut cx = Cx {
            fallback: false,
            unsupported: 0,
        };
        let x = princ(&decoded, &mut cx).unwrap();
        let none = |_: &str| None;
        let md = |_: &str, _: &[String]| None;
        assert!(eval_princ(
            &x,
            &input(
                Some("spiffe://cluster.local/ns/demo/sa/client"),
                b"/",
                &none,
                &md
            )
        ));
        assert!(!eval_princ(
            &x,
            &input(
                Some("spiffe://cluster.local/ns/demo/sa/other"),
                b"/",
                &none,
                &md
            )
        ));
    }

    #[test]
    fn allow_policy_by_principal_and_method() {
        let cfg = RbacCfg {
            action: RbacAction::Allow,
            shadow: false,
            policies: vec![(
                "ns[demo]-policy[allow-get]-rule[0]".into(),
                RbacPolicy {
                    permissions: vec![RbacPermission::Header(HeaderMatch {
                        name: ":method".into(),
                        kind: HeaderMatchKind::Str(StringMatcher::exact("GET")),
                        invert: false,
                        treat_missing_as_empty: false,
                    })],
                    principals: vec![RbacPrincipal::Authenticated(Some(StringMatcher::exact(
                        "spiffe://cluster.local/ns/demo/sa/client",
                    )))],
                },
            )],
        };
        let get = |n: &str| (n == ":method").then(|| bytes::Bytes::from_static(b"GET"));
        let post = |n: &str| (n == ":method").then(|| bytes::Bytes::from_static(b"POST"));
        let md = |_: &str, _: &[String]| None;
        assert!(allowed(
            &cfg,
            &input(
                Some("spiffe://cluster.local/ns/demo/sa/client"),
                b"/",
                &get,
                &md
            )
        ));
        assert!(!allowed(
            &cfg,
            &input(
                Some("spiffe://cluster.local/ns/demo/sa/client"),
                b"/",
                &post,
                &md
            )
        ));
        assert!(!allowed(&cfg, &input(None, b"/", &get, &md)));
        assert!(!allowed(
            &cfg,
            &input(
                Some("spiffe://cluster.local/ns/other/sa/x"),
                b"/",
                &get,
                &md
            )
        ));
    }

    #[test]
    fn deny_policy() {
        let cfg = RbacCfg {
            action: RbacAction::Deny,
            shadow: false,
            policies: vec![(
                "deny-admin".into(),
                RbacPolicy {
                    permissions: vec![RbacPermission::Path(StringMatcher {
                        pattern: StrMatch::Prefix("/admin".into()),
                        ignore_case: false,
                    })],
                    principals: vec![RbacPrincipal::Any],
                },
            )],
        };
        let h = |_: &str| None;
        let md = |_: &str, _: &[String]| None;
        assert!(!allowed(&cfg, &input(None, b"/admin/x", &h, &md)));
        assert!(allowed(&cfg, &input(None, b"/public?a=/admin", &h, &md)));
    }

    fn prefix(p: &str) -> p::StringMatcher {
        p::StringMatcher {
            ignore_case: false,
            pattern: Some(p::string_matcher::Pattern::Prefix(p.into())),
        }
    }

    fn rbac(action: i32, pol: Policy) -> Rbac {
        Rbac {
            action,
            policies: [("p".to_string(), pol)].into_iter().collect(),
        }
    }

    fn any_principal() -> Principal {
        Principal {
            identifier: Some(principal::Identifier::Any(true)),
        }
    }

    /// 評価できない permission（metadata・matcher）は DENY では一致させる（何も拒否しないポリシーにしない）。
    #[test]
    fn unsupported_permission_in_deny_policy_denies() {
        let h = |_: &str| None;
        let md = |_: &str, _: &[String]| None;
        for rule in [
            permission::Rule::Metadata(MetadataMatcher::default()),
            permission::Rule::Matcher(p::Empty::default()),
        ] {
            let pol = Policy {
                permissions: vec![Permission { rule: Some(rule) }],
                principals: vec![any_principal()],
                condition: None,
            };
            let cfg = from_rules(Some(&rbac(1, pol.clone())), None).unwrap();
            assert!(!allowed(&cfg, &input(None, b"/", &h, &md)));
            // ALLOW では一致させない（誰も通さない）
            let cfg = from_rules(Some(&rbac(0, pol)), None).unwrap();
            assert!(!allowed(&cfg, &input(None, b"/", &h, &md)));
        }
    }

    /// 否定の下では安全側が反転する（DENY の `not_rule { metadata }` も一致させる）。
    #[test]
    fn unsupported_permission_under_not_rule_flips() {
        let h = |_: &str| None;
        let md = |_: &str, _: &[String]| None;
        let not_md = Permission {
            rule: Some(permission::Rule::NotRule(Box::new(Permission {
                rule: Some(permission::Rule::Metadata(MetadataMatcher::default())),
            }))),
        };
        let pol = Policy {
            permissions: vec![not_md],
            principals: vec![any_principal()],
            condition: None,
        };
        let cfg = from_rules(Some(&rbac(1, pol.clone())), None).unwrap();
        assert!(!allowed(&cfg, &input(None, b"/", &h, &md)));
        let cfg = from_rules(Some(&rbac(0, pol)), None).unwrap();
        assert!(!allowed(&cfg, &input(None, b"/", &h, &md)));
    }

    /// principal の url_path は全員ではなくパスの照合（ALLOW が全員を通さない）。
    #[test]
    fn principal_url_path_matches_path_only() {
        let h = |_: &str| None;
        let md = |_: &str, _: &[String]| None;
        let pol = Policy {
            permissions: vec![Permission {
                rule: Some(permission::Rule::Any(true)),
            }],
            principals: vec![Principal {
                identifier: Some(principal::Identifier::UrlPath(PathMatcher {
                    path: Some(prefix("/public")),
                })),
            }],
            condition: None,
        };
        let cfg = from_rules(Some(&rbac(0, pol)), None).unwrap();
        assert!(allowed(&cfg, &input(None, b"/public/a", &h, &md)));
        assert!(!allowed(&cfg, &input(None, b"/admin", &h, &md)));
    }
}
