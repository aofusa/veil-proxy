//! 上流（クラスタのエンドポイント）の選択と接続（F-216・F-217・F-219）。
//!
//! - エンドポイントの選択は EDS の集合（`ArcSwap<EndpointSet>`）の読み出しと原子的なカウンタだけ。
//! - 接続はワーカー（スレッド）ごとのプールに持つ（HTTP/1.1 はアイドル接続の再利用、HTTP/2 は多重化）。
//! - TLS はクラスタ（とエンドポイントのメタデータ）で決まる rustls の設定で張る。ALPN は用途
//!   （HTTP/1.1・HTTP/2・TCP）ごとの設定を使い分ける（Istio の `istio.alpn` と同じ）。

use super::httpcore::ResponseFlag;
use super::model::*;
use crate::runtime::handle::AsRawFd;
use crate::runtime::io::{
    AsyncReadRent, AsyncWriteRent, AsyncWriteRentExt, BufResult, BufferedReadState,
};
use crate::runtime::tcp::TcpStream;
use crate::runtime::{IoBuf, IoBufMut};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

pub type ClientTls = crate::pool::ClientTls;

/// 上流の接続（平文か TLS）。
pub enum UpIo {
    Plain(TcpStream),
    Tls(Box<ClientTls>),
    /// Linkerd のプロキシ同士の mTLS（接続の数を Linkerd のメトリクスに数える。F-223）
    #[cfg(feature = "mesh-linkerd")]
    Tracked(Box<ClientTls>, super::linkerd::metrics::TcpGuard),
}

impl AsyncReadRent for UpIo {
    async fn read<T: IoBufMut>(&mut self, buf: T) -> BufResult<usize, T> {
        match self {
            UpIo::Plain(s) => s.read(buf).await,
            UpIo::Tls(s) => s.read(buf).await,
            #[cfg(feature = "mesh-linkerd")]
            UpIo::Tracked(s, _) => s.read(buf).await,
        }
    }
}

impl AsyncWriteRent for UpIo {
    async fn write<T: IoBuf>(&mut self, buf: T) -> BufResult<usize, T> {
        match self {
            UpIo::Plain(s) => s.write(buf).await,
            UpIo::Tls(s) => s.write(buf).await,
            #[cfg(feature = "mesh-linkerd")]
            UpIo::Tracked(s, _) => s.write(buf).await,
        }
    }

    async fn flush(&mut self) -> io::Result<()> {
        match self {
            UpIo::Plain(s) => s.flush().await,
            UpIo::Tls(s) => s.flush().await,
            #[cfg(feature = "mesh-linkerd")]
            UpIo::Tracked(s, _) => s.flush().await,
        }
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        match self {
            UpIo::Plain(s) => AsyncWriteRent::shutdown(s).await,
            UpIo::Tls(s) => s.shutdown().await,
            #[cfg(feature = "mesh-linkerd")]
            UpIo::Tracked(s, _) => s.shutdown().await,
        }
    }
}

impl AsRawFd for UpIo {
    fn as_raw_fd(&self) -> crate::runtime::handle::RawFd {
        match self {
            UpIo::Plain(s) => s.as_raw_fd(),
            UpIo::Tls(s) => s.as_raw_fd(),
            #[cfg(feature = "mesh-linkerd")]
            UpIo::Tracked(s, _) => s.as_raw_fd(),
        }
    }
}

impl BufferedReadState for UpIo {
    fn has_buffered_read_data(&self) -> bool {
        match self {
            UpIo::Plain(_) => false,
            UpIo::Tls(s) => s.has_buffered_read_data(),
            #[cfg(feature = "mesh-linkerd")]
            UpIo::Tracked(s, _) => s.has_buffered_read_data(),
        }
    }
}

/// 選んだエンドポイント。
pub struct Picked {
    pub cfg: Arc<ClusterCfg>,
    pub eps: Arc<EndpointSet>,
    /// `eps.endpoints` の添字（元の宛先へ送るクラスタは `usize::MAX`）
    pub idx: usize,
    pub addr: SocketAddr,
    pub tls: Option<Arc<UpstreamTls>>,
}

impl Picked {
    pub fn endpoint(&self) -> Option<&MeshEndpoint> {
        self.eps.endpoints.get(self.idx)
    }

    /// 同時要求数（LEAST_REQUEST）を数える。
    pub fn begin(&self) {
        if let Some(e) = self.endpoint() {
            e.active.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn end(&self, status_5xx: bool, gateway_error: bool) {
        let Some(e) = self.endpoint() else { return };
        e.active.fetch_sub(1, Ordering::Relaxed);
        // 外れ値検出（連続 5xx / ゲートウェイエラー）
        let Some(o) = &self.cfg.outlier else { return };
        if status_5xx || gateway_error {
            let n = e.consecutive_5xx.fetch_add(1, Ordering::Relaxed) + 1;
            let hit = (o.consecutive_5xx > 0 && n >= o.consecutive_5xx as usize)
                || (gateway_error
                    && o.consecutive_gateway > 0
                    && n >= o.consecutive_gateway as usize);
            if hit {
                let total = self.eps.endpoints.len().max(1);
                let ejected = self
                    .eps
                    .endpoints
                    .iter()
                    .filter(|x| x.is_ejected(super::httpcore::now_ms()))
                    .count();
                if (ejected + 1) * 100 <= total * o.max_ejection_percent.max(1) as usize
                    || ejected == 0
                {
                    let until = super::httpcore::now_ms() + o.base_ejection.as_millis() as u64;
                    e.ejected_until_ms.store(until, Ordering::Relaxed);
                    e.consecutive_5xx.store(0, Ordering::Relaxed);
                }
            }
        } else {
            e.consecutive_5xx.store(0, Ordering::Relaxed);
        }
    }
}

/// クラスタからエンドポイントを選ぶ。`exclude` はリトライで避けるアドレス。
pub fn pick(
    slot: &ClusterSlot,
    orig_dst: Option<SocketAddr>,
    hash: Option<u64>,
    exclude: &[SocketAddr],
) -> Result<Picked, ResponseFlag> {
    let cfg = match slot.cfg.load().as_ref() {
        Some(c) => c.clone(),
        None => return Err(ResponseFlag::NoCluster),
    };
    let eps = slot.endpoints.load_full();
    if cfg.kind == ClusterKind::OriginalDst {
        let addr = orig_dst.ok_or(ResponseFlag::NoHealthyUpstream)?;
        let tls = cfg.tls_default.clone();
        return Ok(Picked {
            cfg,
            eps,
            idx: usize::MAX,
            addr,
            tls,
        });
    }
    if eps.healthy.is_empty() {
        return Err(ResponseFlag::NoHealthyUpstream);
    }
    let now = super::httpcore::now_ms();
    let usable = |i: usize| {
        let e = &eps.endpoints[i];
        !e.is_ejected(now) && !exclude.contains(&e.addr)
    };
    let n = eps.healthy.len();
    let idx = if n == 1 {
        eps.healthy[0]
    } else if let Some(h) = hash {
        // 一貫性ハッシュ（jump consistent hash。リング・Maglev の代わり）
        let b = jump_hash(h, n as i32) as usize;
        eps.healthy[b]
    } else {
        match cfg.lb {
            LbPolicy::LeastRequest => {
                // power of two choices（Envoy の LEAST_REQUEST と同じ）
                let mut best = None;
                for _ in 0..2 {
                    let i = eps.healthy[super::httpcore::rand_u32() as usize % n];
                    if !usable(i) {
                        continue;
                    }
                    let a = eps.endpoints[i].active.load(Ordering::Relaxed);
                    match best {
                        Some((_, ba)) if ba <= a => {}
                        _ => best = Some((i, a)),
                    }
                }
                match best {
                    Some((i, _)) => i,
                    None => rr_pick(&eps, &usable),
                }
            }
            LbPolicy::Random => {
                let i = eps.healthy[super::httpcore::rand_u32() as usize % n];
                if usable(i) { i } else { rr_pick(&eps, &usable) }
            }
            _ => rr_pick(&eps, &usable),
        }
    };
    let ep = &eps.endpoints[idx];
    let tls = match &ep.tls {
        Some(t) => Some(t.clone()),
        None => cfg.tls_for(ep).cloned(),
    };
    // 内部アドレスで元の宛先がメタデータに無いものは、接続の元の宛先を使う（waypoint の direct チェーン）
    let addr = match (&ep.internal, orig_dst) {
        (Some(_), Some(d)) if ep.addr.port() == 0 => d,
        _ => ep.addr,
    };
    Ok(Picked {
        cfg,
        eps,
        idx,
        addr,
        tls,
    })
}

fn rr_pick(eps: &EndpointSet, usable: &dyn Fn(usize) -> bool) -> usize {
    let n = eps.healthy.len();
    // 重み付き（重みが全部 1 なら単純なラウンドロビン）
    let weighted = eps.total_weight != n as u64;
    for _ in 0..n {
        let i = if weighted {
            let mut r = (eps.rr.fetch_add(1, Ordering::Relaxed) as u64) % eps.total_weight.max(1);
            let mut pick = eps.healthy[0];
            for &h in &eps.healthy {
                let w = eps.endpoints[h].weight.max(1) as u64;
                if r < w {
                    pick = h;
                    break;
                }
                r -= w;
            }
            pick
        } else {
            eps.healthy[eps.rr.fetch_add(1, Ordering::Relaxed) % n]
        };
        if usable(i) {
            return i;
        }
    }
    eps.healthy[eps.rr.fetch_add(1, Ordering::Relaxed) % n]
}

fn jump_hash(mut key: u64, buckets: i32) -> i32 {
    let (mut b, mut j) = (-1i64, 0i64);
    while j < buckets as i64 {
        b = j;
        key = key.wrapping_mul(2862933555777941757).wrapping_add(1);
        j = ((b + 1) as f64 * ((1u64 << 31) as f64 / ((key >> 33) + 1) as f64)) as i64;
    }
    b as i32
}

/// サーキットブレーカの入場券（処理中の要求数。Drop で返す）。
pub struct Admission<'a>(&'a ClusterSlot);

impl Drop for Admission<'_> {
    fn drop(&mut self) {
        self.0.stats.active_rq.fetch_sub(1, Ordering::Relaxed);
    }
}

/// クラスタの同時要求数の上限（Envoy の circuit_breakers。HTTP/1.1 は接続数 + 保留、HTTP/2 は
/// 要求数 + 保留）を超えていなければ入場させる。超えていれば `UpstreamOverflow`（503 UO）。
pub fn admit<'a>(slot: &'a ClusterSlot, cfg: &ClusterCfg, h2: bool) -> Result<Admission<'a>, ResponseFlag> {
    let limit = if h2 {
        cfg.max_requests as u64 + cfg.max_pending as u64
    } else {
        cfg.max_connections as u64 + cfg.max_pending as u64
    };
    let prev = slot.stats.active_rq.fetch_add(1, Ordering::Relaxed);
    if prev >= limit {
        slot.stats.active_rq.fetch_sub(1, Ordering::Relaxed);
        return Err(ResponseFlag::UpstreamOverflow);
    }
    slot.stats.upstream_rq_total.fetch_add(1, Ordering::Relaxed);
    Ok(Admission(slot))
}

/// TLS の用途。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Alpn {
    H1,
    H2,
    Tcp,
}

fn ktls_settings() -> (bool, bool, bool) {
    let c = crate::config::CURRENT_CONFIG.load();
    (
        c.ktls_config.enabled,
        c.ktls_config.fallback_enabled,
        c.ktls_config.tcp_cork_enabled,
    )
}

/// 上流へ接続する（TLS ならハンドシェイクまで）。
pub async fn connect(p: &Picked, alpn: Alpn) -> io::Result<UpIo> {
    super::stats::UPSTREAM_CX_TOTAL.fetch_add(1, Ordering::Relaxed);
    // 内部アドレス（Envoy の内部リスナー。ambient の waypoint の HBONE）
    if let Some(name) = p.endpoint().and_then(|e| e.internal.as_ref()) {
        // HBONE の送出は内側でこの関数（mTLS の接続）を呼ぶので Box で再帰を切る
        return Box::pin(super::hbone::connect_internal(name, p.addr))
            .await
            .map(UpIo::Plain);
    }
    // Linkerd の opaque transport: 相手のプロキシの受け口へ ALPN `transport.l5d.io/v1` で張り、
    // 最初に transport header（本来の宛先のポート）を送る
    let tagged = match (p.endpoint().and_then(|e| e.tagged_port), &p.tls) {
        (Some(port), Some(t)) if t.transport.is_some() => Some(port),
        _ => None,
    };
    let dial = match tagged {
        Some(port) => SocketAddr::new(p.addr.ip(), port),
        None => p.addr,
    };
    let tcp = match super::net::connect(dial, p.cfg.bind, p.cfg.connect_timeout).await {
        Ok(s) => s,
        Err(e) => {
            super::stats::UPSTREAM_CX_CONNECT_FAIL.fetch_add(1, Ordering::Relaxed);
            return Err(e);
        }
    };
    let Some(tls) = &p.tls else {
        return Ok(UpIo::Plain(tcp));
    };
    let cfg = match (&tls.transport, tagged, &alpn) {
        (Some(t), Some(_), _) => t.clone(),
        (_, _, Alpn::H1) => tls.h1.clone(),
        (_, _, Alpn::H2) => tls.h2.clone(),
        (_, _, Alpn::Tcp) => tls.tcp.clone(),
    };
    let name = server_name(&tls.sni, p.addr);
    let hs = async {
        #[cfg(veil_ktls)]
        {
            let (ktls, fallback, cork) = ktls_settings();
            crate::ktls_rustls::connect(tcp, cfg, name, ktls, fallback, cork).await
        }
        #[cfg(not(veil_ktls))]
        {
            let _ = ktls_settings;
            crate::simple_tls::connect(tcp, cfg, name).await
        }
    };
    match crate::runtime::timeout(p.cfg.connect_timeout.max(Duration::from_secs(1)), hs).await {
        // Linkerd のプロキシ同士（接続を数える）
        #[cfg(feature = "mesh-linkerd")]
        Ok(Ok(s)) if tls.transport.is_some() => {
            let guard = super::linkerd::metrics::TcpGuard::outbound(
                p.addr,
                &tls.sni,
                p.endpoint().and_then(|e| e.metric_labels.as_deref()),
            );
            let mut io = UpIo::Tracked(Box::new(s), guard);
            if tagged.is_some() {
                let header = linkerd_transport_header(p.addr.port(), &alpn);
                let (r, _) = io.write_all(header).await;
                r?;
            }
            Ok(io)
        }
        Ok(Ok(s)) if tagged.is_some() => {
            let mut io = UpIo::Tls(Box::new(s));
            let header = linkerd_transport_header(p.addr.port(), &alpn);
            let (r, _) = io.write_all(header).await;
            r?;
            Ok(io)
        }
        Ok(Ok(s)) => Ok(UpIo::Tls(Box::new(s))),
        Ok(Err(e)) => {
            super::stats::UPSTREAM_CX_CONNECT_FAIL.fetch_add(1, Ordering::Relaxed);
            Err(e)
        }
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "TLS handshake timeout",
        )),
    }
}

/// Linkerd の transport header（`transport.l5d.io/v1`）。HTTP は中身の版も伝える。
fn linkerd_transport_header(port: u16, alpn: &Alpn) -> Vec<u8> {
    // プレフェース + 長さ（u32 BE） + proto（port = 1: int32、session_protocol = 3: {http1 = 1 | http2 = 2}）
    let mut msg = Vec::with_capacity(16);
    msg.push(0x08);
    let mut v = port as u64;
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            msg.push(b);
            break;
        }
        msg.push(b | 0x80);
    }
    match alpn {
        Alpn::H1 => msg.extend_from_slice(&[0x1a, 0x02, 0x0a, 0x00]),
        Alpn::H2 => msg.extend_from_slice(&[0x1a, 0x02, 0x12, 0x00]),
        Alpn::Tcp => {}
    }
    const PREFACE: &[u8] = b"transport.l5d.io/v1\r\n\r\n";
    let mut out = Vec::with_capacity(PREFACE.len() + 4 + msg.len());
    out.extend_from_slice(PREFACE);
    out.extend_from_slice(&(msg.len() as u32).to_be_bytes());
    out.extend_from_slice(&msg);
    out
}

fn server_name(sni: &str, addr: SocketAddr) -> rustls::pki_types::ServerName<'static> {
    if !sni.is_empty() {
        if let Ok(n) = rustls::pki_types::ServerName::try_from(sni.to_string()) {
            return n;
        }
        // `_` を含む等で DNS 名として通らない SNI は IP に置き換える（SNI は送られない）
    }
    rustls::pki_types::ServerName::IpAddress(addr.ip().into())
}

/// HTTP/2 の接続が ALPN で h2 になったか（平文は常に true＝h2c）。
pub fn negotiated_h2(io: &UpIo) -> bool {
    match io {
        UpIo::Plain(_) => true,
        UpIo::Tls(s) => s.negotiated_h2(),
        #[cfg(feature = "mesh-linkerd")]
        UpIo::Tracked(s, _) => s.negotiated_h2(),
    }
}

// ---------------------------------------------------------------------------
// プール（ワーカーごと）
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct PoolKey {
    addr: SocketAddr,
    /// TLS の設定（`Arc` のポインタ。平文は 0）
    tls: usize,
    bind: Option<SocketAddr>,
    /// 内部リスナー経由（HBONE のトンネル）か
    internal: bool,
}

fn pool_key(p: &Picked) -> PoolKey {
    PoolKey {
        addr: p.addr,
        tls: p.tls.as_ref().map_or(0, |t| Arc::as_ptr(t) as usize),
        bind: p.cfg.bind,
        internal: p.endpoint().is_some_and(|e| e.internal.is_some()),
    }
}

struct Idle {
    io: UpIo,
    since: Instant,
    uses: u32,
}

thread_local! {
    static H1_POOL: RefCell<HashMap<PoolKey, Vec<Idle>>> = RefCell::new(HashMap::new());
    static H2_POOL: RefCell<HashMap<PoolKey, Vec<Rc<crate::http2::upstream_mux::H2Mux>>>> =
        RefCell::new(HashMap::new());
}

const H1_POOL_MAX: usize = 128;

/// 平文の接続が相手に閉じられていないか（未読のデータがあれば使わない。B-93 と同じ考え方）。
fn still_open(io: &UpIo) -> bool {
    let fd = io.as_raw_fd();
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN | libc::POLLRDHUP,
        revents: 0,
    };
    // SAFETY: 1 個の pollfd、待たない
    let r = unsafe { libc::poll(&mut pfd, 1, 0) };
    r == 0
}

/// HTTP/1.1 のアイドル接続を借りる（無ければ `None`）。
pub fn h1_take(p: &Picked) -> Option<(UpIo, u32)> {
    let key = pool_key(p);
    let idle_timeout = p.cfg.idle_timeout;
    H1_POOL.with(|pool| {
        let mut pool = pool.borrow_mut();
        let v = pool.get_mut(&key)?;
        while let Some(i) = v.pop() {
            if i.since.elapsed() >= idle_timeout {
                continue;
            }
            // 1ms 以上アイドルだったものだけ生存確認（B-93）
            if i.since.elapsed() >= Duration::from_millis(1) && !still_open(&i.io) {
                continue;
            }
            return Some((i.io, i.uses));
        }
        None
    })
}

/// HTTP/1.1 の接続を返す。
pub fn h1_put(p: &Picked, io: UpIo, uses: u32) {
    if p.cfg.max_requests_per_conn > 0 && uses >= p.cfg.max_requests_per_conn {
        return;
    }
    let key = pool_key(p);
    H1_POOL.with(|pool| {
        let mut pool = pool.borrow_mut();
        let v = pool.entry(key).or_default();
        if v.len() < H1_POOL_MAX {
            v.push(Idle {
                io,
                since: Instant::now(),
                uses,
            });
        }
    });
}

/// HTTP/2 の多重化接続を得る（空きのある既存の接続か、新しく張る）。
pub(crate) async fn h2_mux(p: &Picked) -> io::Result<Rc<crate::http2::upstream_mux::H2Mux>> {
    let key = pool_key(p);
    let existing = H2_POOL.with(|pool| {
        let mut pool = pool.borrow_mut();
        let v = pool.entry(key).or_default();
        v.retain(|m| !m.is_dead());
        v.iter().find(|m| m.has_capacity()).cloned()
    });
    if let Some(m) = existing {
        return Ok(m);
    }
    let io = connect(p, Alpn::H2).await?;
    // Linkerd のプロキシ同士は ALPN を使わず、HTTP/2 はプリフェースで判別する
    let linkerd = p.tls.as_ref().is_some_and(|t| t.transport.is_some());
    if !negotiated_h2(&io) && !linkerd {
        return Err(io::Error::other("upstream did not negotiate h2"));
    }
    let https = !matches!(io, UpIo::Plain(_));
    let mux = Rc::new(crate::http2::upstream_mux::H2Mux::start(
        io,
        https,
        p.cfg.idle_timeout,
    ));
    if linkerd {
        mux.set_keep_host();
    }
    H2_POOL.with(|pool| pool.borrow_mut().entry(key).or_default().push(mux.clone()));
    Ok(mux)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot_with(eps: Vec<&str>, lb: LbPolicy) -> ClusterSlot {
        let s = ClusterSlot::new("c");
        s.cfg.store(Arc::new(Some(Arc::new(ClusterCfg {
            name: "c".into(),
            kind: ClusterKind::Eds,
            lb,
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
            max_retries: 3,
            service_host: Default::default(),
            service_name: Default::default(),
            service_namespace: Default::default(),
            eds_name: "c".into(),
            svc_labels: None,
            port_override: None,
        }))));
        s.endpoints.store(Arc::new(EndpointSet::new(
            eps.iter()
                .map(|a| MeshEndpoint::new(a.parse().unwrap()))
                .collect(),
        )));
        s
    }

    #[test]
    fn round_robin_and_exclusion() {
        let s = slot_with(vec!["10.0.0.1:80", "10.0.0.2:80"], LbPolicy::RoundRobin);
        let a = pick(&s, None, None, &[]).unwrap().addr;
        let b = pick(&s, None, None, &[]).unwrap().addr;
        assert_ne!(a, b);
        let c = pick(&s, None, None, &[a]).unwrap().addr;
        assert_ne!(c, a);
        let lr = slot_with(vec!["10.0.0.1:80", "10.0.0.2:80"], LbPolicy::LeastRequest);
        let p = pick(&lr, None, None, &[]).unwrap();
        p.begin();
        p.end(false, false);
    }

    #[test]
    fn missing_cluster_and_original_dst() {
        let s = ClusterSlot::new("x");
        assert_eq!(
            pick(&s, None, None, &[]).err(),
            Some(ResponseFlag::NoCluster)
        );
        let e = slot_with(vec![], LbPolicy::RoundRobin);
        assert_eq!(
            pick(&e, None, None, &[]).err(),
            Some(ResponseFlag::NoHealthyUpstream)
        );
    }

    #[test]
    fn jump_hash_is_stable() {
        for k in 0..100u64 {
            let b = jump_hash(k, 7);
            assert!((0..7).contains(&b));
            assert_eq!(b, jump_hash(k, 7));
        }
    }
}
