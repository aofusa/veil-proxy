//! HTTP/1.1・HTTP/2 で共有する L7 の処理（F-219・F-220・F-221）。
//!
//! - ルートの選択（仮想ホスト → ルート → クラスタ。重み付きの分割・ハッシュ）
//! - 認可（RBAC）・フォルト注入・CORS
//! - Envoy と同じ要求・応答ヘッダの書き換え（`x-request-id`・`x-forwarded-proto`・XFCC・
//!   peer metadata・ルートのヘッダ操作・`server`・`x-envoy-upstream-service-time`）

use super::httpcore::{self, ResponseFlag};
use super::model::*;
use super::stats::{Proto, Reporter, ReqLabels};
use bytes::{BufMut, Bytes, BytesMut};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

/// 接続ごとの文脈（接続の確立時に 1 回作る）。
pub struct ConnCtx {
    pub hcm: Arc<HttpCm>,
    pub direction: Direction,
    /// 元の宛先（ORIGINAL_DST のクラスタ・RBAC の宛先）
    pub orig_dst: Option<SocketAddr>,
    pub peer: SocketAddr,
    /// 下流が TLS（メッシュの mTLS）か
    pub tls: bool,
    /// 相手の ID（URI SAN）
    pub peer_principal: Option<Arc<str>>,
    /// 相手の証明書の SHA-256（XFCC の Hash）
    pub peer_cert_hash: Option<Arc<str>>,
    pub peer_dns: Vec<String>,
    pub sni: Option<String>,
    /// LDS の更新でフィルタチェーンを選び直すための接続の属性（静的なリスナーは `None`）
    pub relookup: Option<Arc<Relookup>>,
    /// この文脈を作ったときのリスナーの表の版
    pub table_version: u64,
    /// Istio の標準メトリクスを数えるか（フィルタチェーンに `istio.stats` がある。管理用のリスナー 15090・15021 は数えない）
    pub stats: bool,
}

/// 接続のフィルタチェーンを選び直すのに要る属性。
pub struct Relookup {
    pub laddr: SocketAddr,
    pub dst: SocketAddr,
    pub transport: TransportProto,
    pub alpn: Vec<String>,
}

impl ConnCtx {
    /// リスナーの表が更新されていれば、同じ接続の属性でフィルタチェーンを選び直した文脈を返す
    /// （Envoy は LDS の更新で古いリスナーの接続を排出する。veil は次の要求から新しい設定を使う）。
    /// HTTP でなくなった・リスナーが消えたときは `None`（接続を閉じる）。
    pub fn refreshed(self: &Arc<Self>) -> Option<Arc<ConnCtx>> {
        // Linkerd: ポート（inbound）・宛先（outbound）のポリシーが更新されていれば次の要求から使う
        #[cfg(feature = "mesh-linkerd")]
        if self.hcm.linkerd {
            if let Some(dst) = self.orig_dst {
                if let Some(h) = super::linkerd::discovery::current_hcm(self.direction, dst) {
                    if !Arc::ptr_eq(&h, &self.hcm) {
                        let mut c = self.shallow_clone();
                        c.hcm = h;
                        return Some(Arc::new(c));
                    }
                }
            }
            return Some(self.clone());
        }
        let Some(rl) = &self.relookup else {
            return Some(self.clone());
        };
        let table = super::state::state().listeners.load();
        if table.version == self.table_version {
            return Some(self.clone());
        }
        let lst = table.bound.iter().find(|l| l.addr == rl.laddr)?.clone();
        let target = if lst.use_original_dst {
            table.find_original_dst(rl.dst).cloned().unwrap_or(lst)
        } else {
            lst
        };
        let alpn: Vec<&str> = rl.alpn.iter().map(|s| s.as_str()).collect();
        let facts = ConnFacts {
            dst: Some(rl.dst),
            src: Some(self.peer),
            sni: self.sni.as_deref(),
            transport: Some(rl.transport),
            alpn: &alpn,
        };
        let chain = super::server::select_chain(&target, &facts)?;
        let ChainAction::Http(hcm) = &chain.action else {
            return None;
        };
        Some(Arc::new(ConnCtx {
            hcm: hcm.clone(),
            direction: self.direction,
            orig_dst: self.orig_dst,
            peer: self.peer,
            tls: self.tls,
            peer_principal: self.peer_principal.clone(),
            peer_cert_hash: self.peer_cert_hash.clone(),
            peer_dns: self.peer_dns.clone(),
            sni: self.sni.clone(),
            relookup: self.relookup.clone(),
            table_version: table.version,
            stats: chain.stats,
        }))
    }

    #[cfg(feature = "mesh-linkerd")]
    fn shallow_clone(&self) -> ConnCtx {
        ConnCtx {
            hcm: self.hcm.clone(),
            direction: self.direction,
            orig_dst: self.orig_dst,
            peer: self.peer,
            tls: self.tls,
            peer_principal: self.peer_principal.clone(),
            peer_cert_hash: self.peer_cert_hash.clone(),
            peer_dns: self.peer_dns.clone(),
            sni: self.sni.clone(),
            relookup: self.relookup.clone(),
            table_version: self.table_version,
            stats: self.stats,
        }
    }

    /// 観測のラベル（相手のワークロードは要求ごとに決まる）。
    pub fn labels(
        &self,
        peer: Option<Arc<super::peer::Workload>>,
        svc: Option<Arc<super::stats::ServiceLabels>>,
        grpc: bool,
    ) -> ReqLabels {
        ReqLabels {
            reporter: if self.direction == Direction::Inbound {
                Reporter::Destination
            } else {
                Reporter::Source
            },
            peer,
            peer_principal: self.peer_principal.clone(),
            service: svc,
            proto: if grpc { Proto::Grpc } else { Proto::Http },
            mtls: self.tls && self.peer_principal.is_some(),
        }
    }
}

/// 要求の見方（HTTP/1.1・HTTP/2 共通。ヘッダ名は小文字）。
pub struct ReqView<'a> {
    pub method: &'a [u8],
    /// クエリを含むパス
    pub path: &'a [u8],
    pub authority: &'a [u8],
    pub headers: &'a [(Bytes, Bytes)],
    pub scheme: &'a [u8],
}

impl ReqView<'_> {
    pub fn header(&self, name: &str) -> Option<Bytes> {
        match name {
            ":method" => return Some(Bytes::copy_from_slice(self.method)),
            ":path" => return Some(Bytes::copy_from_slice(self.path)),
            ":authority" | "host" => {
                if !self.authority.is_empty() {
                    return Some(Bytes::copy_from_slice(self.authority));
                }
            }
            ":scheme" => return Some(Bytes::copy_from_slice(self.scheme)),
            _ => {}
        }
        let mut found: Option<Bytes> = None;
        for (k, v) in self.headers {
            if k.eq_ignore_ascii_case(name.as_bytes()) {
                found = Some(match found {
                    None => v.clone(),
                    Some(prev) => {
                        let mut b = BytesMut::with_capacity(prev.len() + v.len() + 1);
                        b.extend_from_slice(&prev);
                        b.put_u8(b',');
                        b.extend_from_slice(v);
                        b.freeze()
                    }
                });
            }
        }
        found
    }

    pub fn is_grpc(&self) -> bool {
        self.headers.iter().any(|(k, v)| {
            k.eq_ignore_ascii_case(b"content-type")
                && v.len() >= 16
                && v[..16].eq_ignore_ascii_case(b"application/grpc")
        })
    }

    fn path_and_query(&self) -> (&[u8], &[u8]) {
        match memchr::memchr(b'?', self.path) {
            Some(i) => (&self.path[..i], &self.path[i + 1..]),
            None => (self.path, &b""[..]),
        }
    }
}

/// ルートの選択の結果。
pub enum Decision {
    Forward(Box<Forward>),
    Local(LocalReply),
}

pub struct LocalReply {
    pub status: u16,
    pub body: Bytes,
    pub headers: Vec<(Bytes, Bytes)>,
    pub flag: ResponseFlag,
    /// gRPC の要求なら grpc-status で返す
    pub grpc_status: Option<u32>,
}

impl LocalReply {
    pub fn new(status: u16, flag: ResponseFlag) -> Self {
        LocalReply {
            status,
            body: Bytes::from_static(httpcore::local_reply_body(status, flag)),
            headers: Vec::new(),
            flag,
            grpc_status: None,
        }
    }
}

pub struct Forward {
    pub table: Arc<RouteTable>,
    pub vh: usize,
    pub route: usize,
    pub action: Arc<ForwardAction>,
    pub cluster: Arc<ClusterSlot>,
    /// 重み付きの分割で選んだ側のヘッダ操作
    pub weighted: Option<usize>,
    pub hash: Option<u64>,
    /// フォルト注入の遅延
    pub delay: Option<Duration>,
    pub cors: Option<Arc<CorsCfg>>,
    /// JWT の検証で決まった要求ヘッダの変更（トークンの削除・claim のヘッダ）
    pub jwt_remove: Vec<Bytes>,
    pub jwt_add: Vec<(Bytes, Bytes)>,
    /// この hop のスパン（トレースの設定があるときだけ）
    pub span: Option<super::tracing::Span>,
    /// 要求の段で通った Wasm のフィルタ（応答の段で呼ぶ）
    #[cfg(feature = "wasm")]
    pub wasm: super::wasm::Ran,
}

impl Forward {
    pub fn route(&self) -> &MeshRoute {
        &self.table.vhosts[self.vh].routes[self.route]
    }
    pub fn vhost(&self) -> &VirtualHost {
        &self.table.vhosts[self.vh]
    }

    /// 応答ヘッダの段の Wasm のフィルタ（無ければ何もしない）。
    pub async fn response_filters(&self, status: u16, headers: &mut Vec<(Bytes, Bytes)>) {
        #[cfg(feature = "wasm")]
        if !self.wasm.is_empty() {
            super::wasm::on_response(&self.wasm, status, headers).await;
        }
        #[cfg(not(feature = "wasm"))]
        let _ = (status, headers);
    }
}

/// RBAC の入力を作って評価する。
fn rbac_allows(
    ctx: &ConnCtx,
    req: &ReqView<'_>,
    cfg: &RbacCfg,
    jwt: &[(String, serde_json::Value)],
) -> bool {
    let header = |n: &str| req.header(n);
    let md = |filter: &str, path: &[String]| -> Option<serde_json::Value> {
        if filter == "envoy.filters.http.jwt_authn" {
            super::jwt::lookup(jwt, path).cloned()
        } else {
            None
        }
    };
    let input = super::xds::rbac::RbacInput {
        header: &header,
        path: req.path,
        dst: ctx.orig_dst,
        sni: ctx.sni.as_deref(),
        principal: ctx.peer_principal.as_deref(),
        direct_remote: Some(ctx.peer.ip()),
        remote: Some(client_address(
            ctx.peer.ip(),
            req.header("x-forwarded-for").as_deref(),
            ctx.hcm.use_remote_address,
            ctx.hcm.xff_num_trusted_hops,
        )),
        metadata: &md,
        http: true,
    };
    super::xds::rbac::allowed(cfg, &input)
}

/// RBAC の remote_ip に使うクライアントのアドレス（Envoy の「元のクライアントのアドレス」と同じ規則。F-236 の F3）。
///
/// - `use_remote_address` あり: 接続の相手。`xff_num_trusted_hops` が N なら X-Forwarded-For の右から N 番目
/// - なし: X-Forwarded-For の右端から `xff_num_trusted_hops` だけ左。ヘッダが無い・足りない・読めないなら接続の相手
///
/// 左端（クライアントが自由に書ける）は使わない（以前は左端を使い、IP の規則をヘッダ 1 つで回避できた）。
fn client_address(
    peer: std::net::IpAddr,
    xff: Option<&[u8]>,
    use_remote_address: bool,
    trusted_hops: u32,
) -> std::net::IpAddr {
    let entries: Vec<&str> = xff
        .and_then(|v| std::str::from_utf8(v).ok())
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|e| !e.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let n = entries.len();
    let hops = trusted_hops as usize;
    let pick = if use_remote_address {
        if hops == 0 || n < hops {
            None
        } else {
            entries.get(n - hops)
        }
    } else if n > hops {
        entries.get(n - 1 - hops)
    } else {
        None
    };
    pick.and_then(|e| e.parse().ok()).unwrap_or(peer)
}

fn chance(per_million: u32) -> bool {
    per_million >= 1_000_000 || (httpcore::rand_u32() % 1_000_000) < per_million
}

/// 要求の段で通った Wasm のフィルタ（`wasm` feature が無ければ空）。
#[cfg(feature = "wasm")]
pub type WasmRan = super::wasm::Ran;
#[cfg(not(feature = "wasm"))]
/// Wasm 無しのビルドの置き物（`()` や単位構造体だと `let ran = WasmRan::default()` が clippy の
/// let_unit_value / default_constructed_unit_structs になる）
#[derive(Default, Clone, Copy, Debug)]
pub struct WasmRan(());

/// 要求の可変の部分（Wasm のフィルタがヘッダ・疑似ヘッダを書き換える）。
pub struct ReqParts<'a> {
    pub method: &'a mut Bytes,
    pub path: &'a mut Bytes,
    pub authority: &'a mut Bytes,
    pub headers: &'a mut Vec<(Bytes, Bytes)>,
    pub scheme: &'a [u8],
    /// 本文が無い（ヘッダで要求が終わる）
    pub eos: bool,
}

impl ReqParts<'_> {
    pub fn view(&self) -> ReqView<'_> {
        ReqView {
            method: self.method,
            path: self.path,
            authority: self.authority,
            headers: self.headers,
            scheme: self.scheme,
        }
    }
}

/// 要求側の認証・認可・Wasm のフィルタ（HTTP フィルタの順）を通す。拒否ならローカル応答。
/// 戻り値は JWT の結果と、応答の段でも呼ぶ Wasm のフィルタ。
pub async fn request_filters(
    ctx: &ConnCtx,
    rq: &mut ReqParts<'_>,
) -> Result<(super::jwt::JwtOutcome, WasmRan), LocalReply> {
    // パスの正規化はルート・RBAC・JWT の照合より前に 1 回だけ。正規化したパスを上流へも送る（F-236 の F5）
    match super::path_norm::normalize(rq.path, &ctx.hcm.path_normalization) {
        Ok(None) => {}
        Ok(Some(p)) => *rq.path = Bytes::from(p),
        Err(super::path_norm::Rejected) => {
            let mut lr = LocalReply::new(400, ResponseFlag::DownstreamProtocolError);
            if rq.view().is_grpc() {
                lr.grpc_status = Some(3);
            }
            return Err(lr);
        }
    }
    let mut jwt = super::jwt::JwtOutcome::default();
    #[cfg_attr(not(feature = "wasm"), allow(unused_mut))]
    let mut ran = WasmRan::default();
    for f in &ctx.hcm.filters {
        let ecds;
        let f = match f {
            HttpFilterCfg::Ecds(slot) => {
                ecds = wait_ecds(slot).await;
                match &ecds {
                    Some(f) => &**f,
                    None => {
                        let mut lr = LocalReply::new(500, ResponseFlag::NoFilterConfigFound);
                        if rq.view().is_grpc() {
                            lr.grpc_status = Some(13);
                        }
                        return Err(lr);
                    }
                }
            }
            f => f,
        };
        match f {
            HttpFilterCfg::Jwt(cfg) => {
                let req = rq.view();
                let header = |n: &str| req.header(n);
                match super::jwt::authenticate(cfg, req.method, req.path, &header) {
                    Ok(o) => {
                        jwt.payloads.extend(o.payloads);
                        jwt.remove_headers.extend(o.remove_headers);
                        jwt.add_headers.extend(o.add_headers);
                    }
                    Err((status, msg)) => {
                        let mut lr = LocalReply::new(status, ResponseFlag::RbacDenied);
                        lr.body = Bytes::from_static(msg.as_bytes());
                        lr.headers.push((
                            Bytes::from_static(b"www-authenticate"),
                            Bytes::from_static(b"Bearer realm=\"veil\", error=\"invalid_token\""),
                        ));
                        if req.is_grpc() {
                            lr.grpc_status = Some(16);
                        }
                        return Err(lr);
                    }
                }
            }
            HttpFilterCfg::Rbac(r) => {
                let req = rq.view();
                if !rbac_allows(ctx, &req, r, &jwt.payloads) {
                    ftlog::debug!(
                        "[mesh] RBAC denied {} {} (principal {:?}, peer {})",
                        String::from_utf8_lossy(req.method),
                        String::from_utf8_lossy(req.path),
                        ctx.peer_principal,
                        ctx.peer
                    );
                    let mut lr = LocalReply::new(403, ResponseFlag::RbacDenied);
                    if req.is_grpc() {
                        lr.grpc_status = Some(7);
                    }
                    return Err(lr);
                }
            }
            #[cfg(feature = "wasm")]
            HttpFilterCfg::Wasm(w) => {
                super::wasm::on_request(w, ctx, rq).await?;
                ran.push(w.clone());
            }
            _ => {}
        }
    }
    Ok((jwt, ran))
}

/// ECDS のフィルタが届くまで少し待つ（Envoy はリスナーのウォーミングで待つ）。
async fn wait_ecds(slot: &EcdsSlot) -> Option<Arc<HttpFilterCfg>> {
    for _ in 0..100 {
        if let Some(f) = slot.filter.load_full() {
            return Some(f);
        }
        crate::runtime::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

/// 要求からルートを選ぶ（フォルト・CORS のプリフライトも含む）。認証・認可は [`request_filters`] で済ませておく。
pub fn decide(
    ctx: &ConnCtx,
    req: &ReqView<'_>,
    jwt: super::jwt::JwtOutcome,
    #[cfg_attr(not(feature = "wasm"), allow(unused_variables))] wasm: WasmRan,
) -> Decision {
    let is_grpc = req.is_grpc();
    let table = ctx.hcm.routes.load();
    let Some(vh) = table.select_vhost(req.authority) else {
        return Decision::Local(LocalReply::new(404, ResponseFlag::NoRoute));
    };
    let vh_idx = table
        .vhosts
        .iter()
        .position(|v| std::ptr::eq(v, vh))
        .unwrap_or(0);
    let (path, query) = req.path_and_query();
    let header = |n: &str| req.header(n);
    let Some(ri) = vh
        .routes
        .iter()
        .position(|r| r.matches(path, query, &header, is_grpc))
    else {
        return Decision::Local(LocalReply::new(404, ResponseFlag::NoRoute));
    };
    let route = &vh.routes[ri];
    // ルート単位の認可（Linkerd の HTTPRoute を対象にした AuthorizationPolicy）
    if let Some(r) = &route.rbac {
        if !rbac_allows(ctx, req, r, &jwt.payloads) {
            let mut lr = LocalReply::new(403, ResponseFlag::RbacDenied);
            if is_grpc {
                lr.grpc_status = Some(7);
            }
            return Decision::Local(lr);
        }
    }
    // CORS（プリフライト）
    let cors = if ctx
        .hcm
        .filters
        .iter()
        .any(|f| matches!(f, HttpFilterCfg::Cors))
    {
        route.cors.clone().or_else(|| vh.cors.clone())
    } else {
        None
    };
    if let Some(c) = &cors {
        if req.method == b"OPTIONS" {
            if let (Some(origin), Some(_)) = (
                req.header("origin"),
                req.header("access-control-request-method"),
            ) {
                if c.allow_origins.iter().any(|m| m.matches(&origin)) {
                    let mut lr = LocalReply::new(200, ResponseFlag::None);
                    lr.body = Bytes::new();
                    lr.headers
                        .push((Bytes::from_static(b"access-control-allow-origin"), origin));
                    if !c.allow_methods.is_empty() {
                        lr.headers.push((
                            Bytes::from_static(b"access-control-allow-methods"),
                            c.allow_methods.clone(),
                        ));
                    }
                    if !c.allow_headers.is_empty() {
                        lr.headers.push((
                            Bytes::from_static(b"access-control-allow-headers"),
                            c.allow_headers.clone(),
                        ));
                    }
                    if !c.max_age.is_empty() {
                        lr.headers.push((
                            Bytes::from_static(b"access-control-max-age"),
                            c.max_age.clone(),
                        ));
                    }
                    if c.allow_credentials {
                        lr.headers.push((
                            Bytes::from_static(b"access-control-allow-credentials"),
                            Bytes::from_static(b"true"),
                        ));
                    }
                    return Decision::Local(lr);
                }
            }
        }
    }
    // フォルト注入
    let mut delay = None;
    if ctx
        .hcm
        .filters
        .iter()
        .any(|f| matches!(f, HttpFilterCfg::Fault(_)))
    {
        let fault = route
            .fault
            .clone()
            .or_else(|| vh.fault.clone())
            .or_else(|| {
                ctx.hcm.filters.iter().find_map(|f| match f {
                    HttpFilterCfg::Fault(f)
                        if f.delay.is_some()
                            || f.abort_http.is_some()
                            || f.abort_grpc.is_some() =>
                    {
                        Some(f.clone())
                    }
                    _ => None,
                })
            });
        if let Some(f) = fault {
            if f.headers
                .iter()
                .all(|h| h.matches(req.header(&h.name).as_deref()))
            {
                if let Some((status, pct)) = f.abort_http {
                    if chance(pct) {
                        let mut lr = LocalReply::new(status, ResponseFlag::FaultInjected);
                        if is_grpc {
                            lr.grpc_status = Some(httpcore_grpc_from_http(status));
                        }
                        return Decision::Local(lr);
                    }
                }
                if let Some((code, pct)) = f.abort_grpc {
                    if chance(pct) {
                        let mut lr = LocalReply::new(200, ResponseFlag::FaultInjected);
                        lr.grpc_status = Some(code);
                        return Decision::Local(lr);
                    }
                }
                if let Some((d, pct)) = f.delay {
                    if chance(pct) {
                        delay = Some(d);
                    }
                }
            }
        }
    }
    match &route.action {
        RouteAction::Direct { status, body } => {
            let mut lr = LocalReply::new(*status, ResponseFlag::None);
            lr.body = body.clone();
            Decision::Local(lr)
        }
        RouteAction::NonForwarding => Decision::Local(LocalReply::new(200, ResponseFlag::None)),
        RouteAction::Redirect(r) => Decision::Local(redirect(r, req, &route.path)),
        RouteAction::Forward(fa) => {
            let (cluster, weighted) = match &fa.target {
                ClusterTarget::Single(c) => (c.clone(), None),
                ClusterTarget::Weighted { clusters, total } => {
                    if clusters.is_empty() {
                        return Decision::Local(LocalReply::new(
                            fa.cluster_not_found,
                            ResponseFlag::NoCluster,
                        ));
                    }
                    let mut r = httpcore::rand_u32() % (*total).max(1);
                    let mut idx = clusters.len() - 1;
                    for (i, c) in clusters.iter().enumerate() {
                        if r < c.weight {
                            idx = i;
                            break;
                        }
                        r -= c.weight;
                    }
                    (clusters[idx].cluster.clone(), Some(idx))
                }
                ClusterTarget::Header(h) => {
                    let Some(name) = req.header(h) else {
                        return Decision::Local(LocalReply::new(
                            fa.cluster_not_found,
                            ResponseFlag::NoCluster,
                        ));
                    };
                    let name = String::from_utf8_lossy(&name).into_owned();
                    let slot = super::state::state()
                        .registry
                        .lock()
                        .ok()
                        .and_then(|r| r.clusters.get(&name).cloned());
                    match slot {
                        Some(s) => (s, None),
                        None => {
                            return Decision::Local(LocalReply::new(
                                fa.cluster_not_found,
                                ResponseFlag::NoCluster,
                            ));
                        }
                    }
                }
            };
            // クラスタが（まだ・もう）無い。Istio は Gateway API の無効な backendRef をこの形で送る
            // （既定の 503 は従来どおり上流を選ぶところで返す。Linkerd は解決を待つため）
            if fa.cluster_not_found != 503 && cluster.cfg.load().is_none() {
                return Decision::Local(LocalReply::new(
                    fa.cluster_not_found,
                    ResponseFlag::NoCluster,
                ));
            }
            let hash = hash_of(&fa.hash, req, ctx);
            Decision::Forward(Box::new(Forward {
                table: table.clone(),
                vh: vh_idx,
                route: ri,
                action: fa.clone(),
                cluster,
                weighted,
                hash,
                delay,
                cors,
                jwt_remove: jwt.remove_headers,
                jwt_add: jwt.add_headers,
                #[cfg(feature = "wasm")]
                wasm,
                span: ctx.hcm.tracer.as_ref().map(|t| {
                    let header = |n: &str| req.header(n);
                    super::tracing::begin(t, &header)
                }),
            }))
        }
    }
}

fn httpcore_grpc_from_http(status: u16) -> u32 {
    match status {
        400 => 13,
        401 => 16,
        403 => 7,
        404 => 12,
        429 | 502 | 503 | 504 => 14,
        _ => 2,
    }
}

fn hash_of(policies: &[HashPolicy], req: &ReqView<'_>, ctx: &ConnCtx) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    if policies.is_empty() {
        return None;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let mut any = false;
    for p in policies {
        match p {
            HashPolicy::Header(n) => {
                if let Some(v) = req.header(n) {
                    v.hash(&mut h);
                    any = true;
                }
            }
            HashPolicy::Cookie(n) => {
                if let Some(c) = req.header("cookie") {
                    for part in c.split(|&b| b == b';') {
                        let part = part.trim_ascii();
                        if part.len() > n.len()
                            && part.starts_with(n.as_bytes())
                            && part[n.len()] == b'='
                        {
                            part[n.len() + 1..].hash(&mut h);
                            any = true;
                        }
                    }
                }
            }
            HashPolicy::SourceIp => {
                ctx.peer.ip().hash(&mut h);
                any = true;
            }
            HashPolicy::Query(n) => {
                let (_, q) = req.path_and_query();
                if let Some(v) = query_value(q, n.as_bytes()) {
                    v.hash(&mut h);
                    any = true;
                }
            }
        }
    }
    any.then(|| h.finish())
}

fn redirect(r: &RedirectCfg, req: &ReqView<'_>, matched: &PathMatch) -> LocalReply {
    let scheme = r.scheme.clone().unwrap_or_else(|| {
        String::from_utf8_lossy(if req.scheme.is_empty() {
            b"http"
        } else {
            req.scheme
        })
        .into_owned()
    });
    let host_full = String::from_utf8_lossy(req.authority).into_owned();
    let (host0, port0) = match host_full.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => {
            (h.to_string(), Some(p.to_string()))
        }
        _ => (host_full.clone(), None),
    };
    // Envoy と同じ: port_redirect があればそれ。host_redirect か scheme を変えたら元のポートは引き継がない
    let port = match (r.port, &r.scheme, &r.host) {
        (Some(p), _, _) => Some(p.to_string()),
        (None, Some(_), _) | (None, None, Some(_)) => None,
        (None, None, None) => port0,
    };
    let host = r.host.clone().unwrap_or(host0);
    let (path, query) = req.path_and_query();
    let mut new_path = match (&r.path, &r.prefix_rewrite) {
        (Some(p), _) => p.clone(),
        (None, Some(pre)) => {
            // 一致した prefix を置き換える（Envoy の prefix_rewrite。完全一致・正規表現の一致はパス全体）
            let rest: &[u8] = match matched {
                PathMatch::Prefix(p) | PathMatch::Separated(p) if path.len() >= p.len() => {
                    &path[p.len()..]
                }
                PathMatch::Exact(_) | PathMatch::Regex(_) => b"",
                _ => path,
            };
            let rest = String::from_utf8_lossy(rest);
            if pre.ends_with('/') && rest.starts_with('/') {
                format!("{}{}", pre, &rest[1..])
            } else if rest.is_empty() || rest.starts_with('/') || pre.ends_with('/') {
                format!("{}{}", pre, rest)
            } else {
                format!("{}/{}", pre, rest)
            }
        }
        (None, None) => String::from_utf8_lossy(path).into_owned(),
    };
    if !r.strip_query && !query.is_empty() && !new_path.contains('?') {
        new_path.push('?');
        new_path.push_str(&String::from_utf8_lossy(query));
    }
    // 方式の既定のポート（http の 80・https の 443）は書かない（Envoy と同じ）
    let port = port.filter(|p| {
        !((scheme.eq_ignore_ascii_case("http") && p == "80")
            || (scheme.eq_ignore_ascii_case("https") && p == "443"))
    });
    let loc = match port {
        Some(p) => format!("{}://{}:{}{}", scheme, host, p, new_path),
        None => format!("{}://{}{}", scheme, host, new_path),
    };
    let mut lr = LocalReply::new(r.status, ResponseFlag::None);
    lr.body = Bytes::new();
    lr.headers
        .push((Bytes::from_static(b"location"), Bytes::from(loc)));
    lr
}

/// 上流へ送るパス（prefix・正規表現の書き換え）。
pub fn rewritten_path(f: &Forward, path: &[u8]) -> Option<Bytes> {
    let a = &f.action;
    if let Some(pre) = &a.prefix_rewrite {
        if let PathMatch::Prefix(p) | PathMatch::Separated(p) = &f.route().path {
            if path.len() >= p.len() {
                let mut b = BytesMut::with_capacity(pre.len() + path.len());
                b.extend_from_slice(pre);
                let rest = &path[p.len()..];
                if pre.ends_with(b"/") && rest.starts_with(b"/") {
                    b.extend_from_slice(&rest[1..]);
                } else {
                    b.extend_from_slice(rest);
                }
                return Some(b.freeze());
            }
        }
        // 完全一致・正規表現の一致は、クエリを除いたパス全体を置き換える（Envoy と同じ。Istio は正規表現の
        // 一致の `rewrite.uri` を `prefix_rewrite` で送る）
        if let PathMatch::Exact(_) | PathMatch::Regex(_) = &f.route().path {
            let q = memchr::memchr(b'?', path)
                .map(|i| &path[i..])
                .unwrap_or(b"");
            let mut b = BytesMut::with_capacity(pre.len() + q.len());
            b.extend_from_slice(pre);
            b.extend_from_slice(q);
            return Some(b.freeze());
        }
        if matches!(f.route().path, PathMatch::Any) {
            let mut b = BytesMut::with_capacity(pre.len() + path.len());
            b.extend_from_slice(pre);
            b.extend_from_slice(
                path.strip_prefix(b"/")
                    .filter(|_| pre.ends_with(b"/"))
                    .unwrap_or(path),
            );
            return Some(b.freeze());
        }
    }
    if let Some((re, sub)) = &a.regex_rewrite {
        let (p, q) = match memchr::memchr(b'?', path) {
            Some(i) => (&path[..i], &path[i..]),
            None => (path, &b""[..]),
        };
        if let Ok(s) = std::str::from_utf8(p) {
            let sub = sub.replace("\\", "$");
            let out = re.replace_all(s, sub.as_str());
            let mut b = BytesMut::from(out.as_bytes());
            b.extend_from_slice(q);
            return Some(b.freeze());
        }
    }
    None
}

/// XFCC の値（`By=...;Hash=...;Subject="";URI=...`）。
fn xfcc_element(ctx: &ConnCtx) -> Option<String> {
    let hash = ctx.peer_cert_hash.as_ref()?;
    let by = super::identity::cert("default")
        .map(|c| c.uri.clone())
        .unwrap_or_default();
    let mut s = String::with_capacity(256);
    if !by.is_empty() {
        s.push_str("By=");
        s.push_str(&by);
        s.push(';');
    }
    s.push_str("Hash=");
    s.push_str(hash);
    if ctx.hcm.xfcc_subject {
        s.push_str(";Subject=\"\"");
    }
    if ctx.hcm.xfcc_uri {
        if let Some(p) = &ctx.peer_principal {
            s.push_str(";URI=");
            s.push_str(p);
        }
    }
    if ctx.hcm.xfcc_dns {
        for d in &ctx.peer_dns {
            s.push_str(";DNS=");
            s.push_str(d);
        }
    }
    Some(s)
}

/// 上流へ送る要求ヘッダを作る（ホップバイホップを除いた下流のヘッダに Envoy の書き換えを足す）。
/// `attempt` は 1 から。戻り値は `(ヘッダ, 要求の相手のワークロード（inbound の peer metadata）)`。
pub fn upstream_request_headers(
    ctx: &ConnCtx,
    f: &Forward,
    headers: &[(Bytes, Bytes)],
    attempt: u32,
    keep_upgrade: bool,
    keep_chunked: bool,
) -> (Vec<(Bytes, Bytes)>, Option<Arc<super::peer::Workload>>) {
    let mut out: Vec<(Bytes, Bytes)> = Vec::with_capacity(headers.len() + 6);
    let mut has_req_id = false;
    let mut has_xfp = false;
    let mut xfcc: Option<Bytes> = None;
    let mut peer_md: Option<Bytes> = None;
    let mut peer_md_id: Option<Bytes> = None;
    for (k, v) in headers {
        let kb = k.as_ref();
        if kb.starts_with(b":") {
            continue;
        }
        if httpcore::is_hop_by_hop(kb) {
            let keep = (keep_upgrade
                && (kb.eq_ignore_ascii_case(b"upgrade") || kb.eq_ignore_ascii_case(b"connection")))
                || (kb.eq_ignore_ascii_case(b"te") && v.eq_ignore_ascii_case(b"trailers"))
                // chunked の本文はそのまま中継するので Transfer-Encoding も渡す（HTTP/1.1 の上流）
                || (kb.eq_ignore_ascii_case(b"transfer-encoding") && keep_chunked);
            if !keep {
                continue;
            }
        }
        if kb.eq_ignore_ascii_case(b"x-request-id") {
            has_req_id = true;
        } else if kb.eq_ignore_ascii_case(b"x-forwarded-proto") {
            has_xfp = true;
        } else if kb.eq_ignore_ascii_case(b"x-forwarded-client-cert") {
            xfcc = Some(v.clone());
            continue;
        } else if kb.eq_ignore_ascii_case(super::peer::HDR_METADATA) {
            peer_md = Some(v.clone());
            continue;
        } else if kb.eq_ignore_ascii_case(super::peer::HDR_METADATA_ID) {
            peer_md_id = Some(v.clone());
            continue;
        } else if kb.eq_ignore_ascii_case(b"x-envoy-attempt-count")
            || kb.eq_ignore_ascii_case(b"expect")
            || (ctx.hcm.linkerd
                && (kb.eq_ignore_ascii_case(b"l5d-client-id")
                    || kb.eq_ignore_ascii_case(b"l5d-orig-proto")))
        {
            continue;
        }
        out.push((k.clone(), v.clone()));
    }
    if ctx.hcm.use_remote_address {
        let ip = ctx.peer.ip().to_string();
        match out
            .iter_mut()
            .find(|(k, _)| k.eq_ignore_ascii_case(b"x-forwarded-for"))
        {
            Some((_, v)) => {
                let mut nv = BytesMut::with_capacity(v.len() + ip.len() + 2);
                nv.extend_from_slice(v);
                nv.extend_from_slice(b", ");
                nv.extend_from_slice(ip.as_bytes());
                *v = nv.freeze();
            }
            None => out.push((Bytes::from_static(b"x-forwarded-for"), Bytes::from(ip))),
        }
    }
    if !has_req_id && ctx.hcm.generate_request_id {
        out.push((Bytes::from_static(b"x-request-id"), httpcore::uuid_v4()));
    }
    // Linkerd の inbound: mTLS の相手の ID をアプリへ渡す（本家の linkerd2-proxy と同じ）
    if ctx.hcm.linkerd && ctx.direction == Direction::Inbound {
        let id = ctx
            .peer_dns
            .first()
            .map(|d| Bytes::from(d.clone()))
            .or_else(|| {
                ctx.peer_principal
                    .as_ref()
                    .map(|p| Bytes::from(p.to_string()))
            });
        if let Some(id) = id {
            out.push((Bytes::from_static(b"l5d-client-id"), id));
        }
    }
    if !has_xfp && !ctx.hcm.linkerd {
        out.push((
            Bytes::from_static(b"x-forwarded-proto"),
            Bytes::from_static(if ctx.tls && ctx.direction != Direction::Inbound {
                b"https"
            } else {
                b"http"
            }),
        ));
    }
    // XFCC
    let elem = if ctx.tls { xfcc_element(ctx) } else { None };
    match ctx.hcm.xfcc {
        XfccMode::Sanitize => {}
        XfccMode::ForwardOnly | XfccMode::AlwaysForwardOnly => {
            if let Some(x) = xfcc {
                if ctx.tls || ctx.hcm.xfcc == XfccMode::AlwaysForwardOnly {
                    out.push((Bytes::from_static(b"x-forwarded-client-cert"), x));
                }
            }
        }
        XfccMode::AppendForward => {
            let v = match (xfcc, elem) {
                (Some(x), Some(e)) => Some(Bytes::from(format!(
                    "{},{}",
                    String::from_utf8_lossy(&x),
                    e
                ))),
                (None, Some(e)) => Some(Bytes::from(e)),
                (Some(x), None) if ctx.tls => Some(x),
                _ => None,
            };
            if let Some(v) = v {
                out.push((Bytes::from_static(b"x-forwarded-client-cert"), v));
            }
        }
        XfccMode::SanitizeSet => {
            if let Some(e) = elem {
                out.push((
                    Bytes::from_static(b"x-forwarded-client-cert"),
                    Bytes::from(e),
                ));
            }
        }
    }
    // peer metadata
    let mut peer = None;
    if ctx.hcm.peer_metadata {
        match ctx.direction {
            Direction::Inbound => {
                if let Some(v) = &peer_md {
                    peer = super::peer::cached_decode(peer_md_id.as_ref(), v);
                }
            }
            _ => {
                if let Some(l) = super::peer::local() {
                    out.push((
                        Bytes::from_static(super::peer::HDR_METADATA),
                        l.header_value.clone(),
                    ));
                    out.push((
                        Bytes::from_static(super::peer::HDR_METADATA_ID),
                        l.header_id.clone(),
                    ));
                }
            }
        }
    }
    if f.vhost().include_attempt_count {
        let mut b = itoa::Buffer::new();
        out.push((
            Bytes::from_static(b"x-envoy-attempt-count"),
            Bytes::copy_from_slice(b.format(attempt).as_bytes()),
        ));
    }
    if let Some(sp) = &f.span {
        out.retain(|(k, _)| {
            !(k.eq_ignore_ascii_case(b"x-b3-traceid")
                || k.eq_ignore_ascii_case(b"x-b3-spanid")
                || k.eq_ignore_ascii_case(b"x-b3-parentspanid")
                || k.eq_ignore_ascii_case(b"x-b3-sampled")
                || k.eq_ignore_ascii_case(b"b3"))
        });
        out.extend(sp.upstream_headers());
    }
    for r in &f.jwt_remove {
        out.retain(|(k, _)| !k.eq_ignore_ascii_case(r));
    }
    for (k, v) in &f.jwt_add {
        out.retain(|(n, _)| !n.eq_ignore_ascii_case(k));
        out.push((k.clone(), v.clone()));
    }
    // ヘッダ操作（重み付き → ルート → 仮想ホスト → 表 の順。Envoy は具体的な方を後に適用＝勝つ）
    f.table.request_ops.apply(&mut out);
    f.vhost().request_ops.apply(&mut out);
    f.route().request_ops.apply(&mut out);
    if let (Some(i), ClusterTarget::Weighted { clusters, .. }) = (f.weighted, &f.action.target) {
        clusters[i].request_ops.apply(&mut out);
    }
    (out, peer)
}

/// 下流へ返す応答ヘッダの書き換え。`resp` は上流の応答ヘッダ（小文字）。戻り値は上流のワークロード
/// （outbound の peer metadata）。
pub fn downstream_response_headers(
    ctx: &ConnCtx,
    f: Option<&Forward>,
    resp: &mut Vec<(Bytes, Bytes)>,
    upstream_ms: Option<u64>,
    origin: Option<&Bytes>,
) -> Option<Arc<super::peer::Workload>> {
    let mut peer = None;
    let mut peer_md: Option<Bytes> = None;
    let mut peer_md_id: Option<Bytes> = None;
    if ctx.hcm.linkerd {
        // Linkerd は応答を書き換えない（outbound で HTTP/2 へ昇格したときの `l5d-orig-proto` だけ外す）
        if ctx.direction != Direction::Inbound {
            resp.retain(|(k, _)| !k.eq_ignore_ascii_case(b"l5d-orig-proto"));
        }
        if let Some(f) = f {
            f.route().response_ops.apply(resp);
        }
        return None;
    }
    resp.retain(|(k, v)| {
        if k.eq_ignore_ascii_case(super::peer::HDR_METADATA) {
            peer_md = Some(v.clone());
            return false;
        }
        if k.eq_ignore_ascii_case(super::peer::HDR_METADATA_ID) {
            peer_md_id = Some(v.clone());
            return false;
        }
        !k.eq_ignore_ascii_case(b"server")
            && !k.eq_ignore_ascii_case(b"x-envoy-upstream-service-time")
    });
    if ctx.hcm.peer_metadata {
        match ctx.direction {
            Direction::Inbound => {
                if let Some(l) = super::peer::local() {
                    resp.push((
                        Bytes::from_static(super::peer::HDR_METADATA),
                        l.header_value.clone(),
                    ));
                    resp.push((
                        Bytes::from_static(super::peer::HDR_METADATA_ID),
                        l.header_id.clone(),
                    ));
                }
            }
            _ => {
                if let Some(v) = &peer_md {
                    peer = super::peer::cached_decode(peer_md_id.as_ref(), v);
                }
            }
        }
    }
    resp.push((Bytes::from_static(b"server"), ctx.hcm.server_name.clone()));
    if let Some(ms) = upstream_ms {
        let mut b = itoa::Buffer::new();
        resp.push((
            Bytes::from_static(b"x-envoy-upstream-service-time"),
            Bytes::copy_from_slice(b.format(ms).as_bytes()),
        ));
    }
    if let Some(f) = f {
        if let (Some(i), ClusterTarget::Weighted { clusters, .. }) = (f.weighted, &f.action.target)
        {
            clusters[i].response_ops.apply(resp);
        }
        f.route().response_ops.apply(resp);
        f.vhost().response_ops.apply(resp);
        f.table.response_ops.apply(resp);
        if let (Some(c), Some(origin)) = (&f.cors, origin) {
            if c.allow_origins.iter().any(|m| m.matches(origin)) {
                resp.push((
                    Bytes::from_static(b"access-control-allow-origin"),
                    origin.clone(),
                ));
                if c.allow_credentials {
                    resp.push((
                        Bytes::from_static(b"access-control-allow-credentials"),
                        Bytes::from_static(b"true"),
                    ));
                }
                if !c.expose_headers.is_empty() {
                    resp.push((
                        Bytes::from_static(b"access-control-expose-headers"),
                        c.expose_headers.clone(),
                    ));
                }
            }
        }
    }
    peer
}

/// 上流のプロトコル（クラスタの設定と下流のプロトコル）。
pub fn upstream_is_h2(cfg: &ClusterCfg, downstream_h2: bool) -> bool {
    match cfg.proto {
        UpstreamProto::Http2 => true,
        UpstreamProto::Http1 => false,
        UpstreamProto::UseDownstream => downstream_h2,
    }
}

/// リトライするか（応答のステータス・gRPC のステータス）。
pub fn retry_on_status(rp: &RetryPolicy, status: u16, grpc: Option<u32>) -> bool {
    (rp.on_5xx && status >= 500)
        || (rp.on_gateway_error && matches!(status, 502..=504))
        || (rp.on_retriable_status && rp.retriable_status_codes.contains(&status))
        || grpc.is_some_and(|g| rp.grpc_codes.contains(&g))
}

/// 要求の相手（観測）を決める: inbound は要求の peer metadata、outbound は応答の peer metadata か EDS の属性。
pub fn peer_workload(
    ctx: &ConnCtx,
    from_request: Option<Arc<super::peer::Workload>>,
    from_response: Option<Arc<super::peer::Workload>>,
    endpoint: Option<&MeshEndpoint>,
) -> Option<Arc<super::peer::Workload>> {
    match ctx.direction {
        // ヘッダが無ければ送信元の IP から（Envoy の workload_discovery と同じ）
        Direction::Inbound => from_request.or_else(|| super::peer::by_ip(ctx.peer.ip())),
        _ => from_response
            .or_else(|| endpoint.and_then(|e| super::peer::cached_endpoint(&e.workload))),
    }
}

/// HTTP の要求 1 件のアクセスログ（設定があるときだけ。`ctx.hcm.access_logs`）。
#[allow(clippy::too_many_arguments)]
pub fn access_log(
    ctx: &ConnCtx,
    req: &ReqView<'_>,
    start: std::time::SystemTime,
    protocol: &str,
    status: u16,
    flag: ResponseFlag,
    bytes_received: u64,
    bytes_sent: u64,
    duration_ms: u64,
    upstream: Option<SocketAddr>,
    cluster: Option<&ClusterCfg>,
    route_name: &str,
    upstream_ms: Option<u64>,
) {
    if ctx.hcm.access_logs.is_empty() {
        return;
    }
    let reqf = |n: &str| req.header(n);
    let respf = |n: &str| -> Option<Bytes> {
        if n == "x-envoy-upstream-service-time" {
            upstream_ms.map(|m| Bytes::from(m.to_string()))
        } else {
            None
        }
    };
    let details = match flag {
        ResponseFlag::None if upstream.is_some() => "via_upstream",
        ResponseFlag::None => "direct_response",
        ResponseFlag::NoRoute => "route_not_found",
        ResponseFlag::RbacDenied => "rbac_access_denied_matched_policy[none]",
        ResponseFlag::FaultInjected => "fault_filter_abort",
        ResponseFlag::NoHealthyUpstream => "no_healthy_upstream",
        ResponseFlag::UpstreamOverflow => "upstream_reset_before_response_started{overflow}",
        ResponseFlag::UpstreamTimeout => "response_timeout",
        ResponseFlag::NoCluster => "cluster_not_found",
        _ => "upstream_reset_before_response_started{connection_failure}",
    };
    let rec = super::accesslog::Record {
        start,
        protocol,
        code: status,
        flags: flag.as_str(),
        details,
        bytes_received,
        bytes_sent,
        duration_ms,
        req: &reqf,
        resp: &respf,
        upstream_host: upstream,
        cluster: cluster.map(|c| c.name.as_str()).unwrap_or(""),
        downstream_local: ctx.orig_dst,
        downstream_remote: Some(ctx.peer),
        sni: ctx.sni.as_deref(),
        route_name,
    };
    super::accesslog::emit(&ctx.hcm.access_logs, &rec);
}

/// この hop のスパンを終える（トレースの設定があり、サンプルしたときだけ送る）。
pub fn finish_span(
    ctx: &ConnCtx,
    f: &Forward,
    req: &ReqView<'_>,
    status: u16,
    cluster: Option<&ClusterCfg>,
) {
    let (Some(t), Some(sp)) = (&ctx.hcm.tracer, &f.span) else {
        return;
    };
    if !sp.sampled {
        return;
    }
    let op = f.route().operation.clone();
    let name = if op.is_empty() {
        String::from_utf8_lossy(req.authority).into_owned()
    } else {
        String::from_utf8_lossy(&op).into_owned()
    };
    let url = format!(
        "{}://{}{}",
        String::from_utf8_lossy(if req.scheme.is_empty() {
            b"http"
        } else {
            req.scheme
        }),
        String::from_utf8_lossy(req.authority),
        String::from_utf8_lossy(req.path)
    );
    let kind = if ctx.direction == Direction::Inbound {
        "SERVER"
    } else {
        "CLIENT"
    };
    let header = |n: &str| req.header(n);
    super::tracing::finish(
        t,
        sp,
        &name,
        kind,
        req.method,
        &url,
        status,
        cluster.map(|c| c.name.as_str()).unwrap_or(""),
        &header,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_address_never_uses_leftmost_xff() {
        let peer: std::net::IpAddr = "10.9.9.9".parse().unwrap();
        let xff = Some(&b"1.1.1.1, 2.2.2.2, 3.3.3.3"[..]);
        // 既定: 右端
        assert_eq!(
            client_address(peer, xff, false, 0),
            "3.3.3.3".parse::<std::net::IpAddr>().unwrap()
        );
        assert_eq!(
            client_address(peer, xff, false, 1),
            "2.2.2.2".parse::<std::net::IpAddr>().unwrap()
        );
        // 足りなければ接続の相手
        assert_eq!(client_address(peer, xff, false, 3), peer);
        // use_remote_address: 相手、または右から N 番目
        assert_eq!(client_address(peer, xff, true, 0), peer);
        assert_eq!(
            client_address(peer, xff, true, 1),
            "3.3.3.3".parse::<std::net::IpAddr>().unwrap()
        );
        assert_eq!(client_address(peer, None, false, 0), peer);
        assert_eq!(client_address(peer, Some(&b"junk"[..]), false, 0), peer);
    }

    fn loc(r: &RedirectCfg, authority: &str, path: &str, m: &PathMatch) -> String {
        let req = ReqView {
            method: b"GET",
            path: path.as_bytes(),
            authority: authority.as_bytes(),
            headers: &[],
            scheme: b"http",
        };
        let lr = redirect(r, &req, m);
        let v = &lr
            .headers
            .iter()
            .find(|(k, _)| k.as_ref() == b"location")
            .unwrap()
            .1;
        String::from_utf8(v.to_vec()).unwrap()
    }

    fn cfg() -> RedirectCfg {
        RedirectCfg {
            host: None,
            port: None,
            scheme: None,
            path: None,
            prefix_rewrite: None,
            strip_query: false,
            status: 302,
        }
    }

    /// Gateway API の ReplacePrefixMatch（Envoy の prefix_rewrite）は一致した prefix だけを置き換える。
    #[test]
    fn redirect_replaces_only_the_matched_prefix() {
        let mut r = cfg();
        r.prefix_rewrite = Some("/replacement-prefix".into());
        let m = PathMatch::Separated("/original-prefix".into());
        assert_eq!(
            loc(&r, "gw", "/original-prefix/lemon", &m),
            "http://gw/replacement-prefix/lemon"
        );
        assert_eq!(
            loc(&r, "gw", "/original-prefix", &m),
            "http://gw/replacement-prefix"
        );
        assert_eq!(
            loc(&r, "gw", "/original-prefix?a=1", &m),
            "http://gw/replacement-prefix?a=1"
        );
    }

    /// host_redirect だけなら元のポートは引き継がない。既定のポートは書かない。port_redirect は付ける。
    #[test]
    fn redirect_port_follows_envoy() {
        let mut r = cfg();
        r.host = Some("example.org".into());
        assert_eq!(
            loc(&r, "gw:8080", "/x", &PathMatch::Any),
            "http://example.org/x"
        );
        r.port = Some(80);
        assert_eq!(
            loc(&r, "gw:8080", "/x", &PathMatch::Any),
            "http://example.org/x"
        );
        r.port = Some(8443);
        assert_eq!(
            loc(&r, "gw:8080", "/x", &PathMatch::Any),
            "http://example.org:8443/x"
        );
        let mut r = cfg();
        r.path = Some("/y".into());
        assert_eq!(
            loc(&r, "gw:8080", "/x", &PathMatch::Any),
            "http://gw:8080/y"
        );
    }
}
