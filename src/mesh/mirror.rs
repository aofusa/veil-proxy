//! 要求のミラー（シャドー。F-219: Istio の VirtualService の `mirror`・Envoy の `request_mirror_policies`）。
//!
//! 本文を手元に全部持っている要求だけを、別のタスクで HTTP/1.1 のままミラー先のクラスタへ送る。応答は待たずに
//! 捨てる（本線の応答に影響させない）。Host には Envoy と同じく `-shadow` を付ける（Istio は
//! `disable_shadow_host_suffix_append` で付けない）。ミラー先の同時実行は
//! サーキットブレーカ（`upstream::admit`）で上限を掛ける。

use super::l7::{ConnCtx, Forward};
use super::upstream::{self, Alpn};
use crate::runtime::io::{AsyncReadRent, AsyncWriteRentExt};
use bytes::{BufMut, Bytes, BytesMut};
use std::sync::Arc;
use std::time::Duration;

#[allow(clippy::too_many_arguments)]
pub fn spawn_h1(
    f: &Forward,
    ctx: &ConnCtx,
    method: &[u8],
    path: &[u8],
    authority: &[u8],
    headers: &[(Bytes, Bytes)],
    body: Bytes,
    chunked: bool,
) {
    // B-394: ミラーにも本線と同じ加工をしたヘッダを送る（XFCC・peer metadata・l5d-client-id の扱い、JWT の
    // `forward = false`、ルートのヘッダ操作）。以前は下流のヘッダをそのまま写していて、本線では消す偽の XFCC・
    // トークンがミラー先へ届いた。送るミラーがあるときだけ 1 回作る
    let mut sanitized: Option<Vec<(Bytes, Bytes)>> = None;
    for (slot, fraction, suffix) in &f.action.mirrors {
        if *fraction < 1_000_000 && super::httpcore::rand_u32() % 1_000_000 >= *fraction {
            continue;
        }
        let headers = sanitized.get_or_insert_with(|| mirror_headers(ctx, f, headers, chunked));
        let Ok(mut picked) = upstream::pick(slot, ctx.orig_dst, None, &[]) else {
            continue;
        };
        // B-375: auto_sni のクラスタは元の要求の Host を TLS の名前にする（`-shadow` を付ける前の名前）
        if picked.cfg.auto_sni {
            picked = picked.with_request_host(&Bytes::copy_from_slice(authority));
        }
        let head = mirror_head(method, path, authority, headers, *suffix, chunked);
        let body = body.clone();
        let slot: Arc<super::model::ClusterSlot> = slot.clone();
        crate::runtime::spawn(async move {
            let Ok(_a) = upstream::admit(&slot, &picked.cfg, false) else {
                return;
            };
            let fut = async {
                let mut up = upstream::connect(&picked, Alpn::H1).await.ok()?;
                up.write_all(head).await.0.ok()?;
                if !body.is_empty() {
                    up.write_all(body).await.0.ok()?;
                }
                // 応答の先頭まで読んで閉じる
                let b: Vec<u8> = Vec::with_capacity(4096);
                let _ = up.read(b).await;
                Some(())
            };
            let _ = crate::runtime::timeout(Duration::from_secs(10), fut).await;
        });
    }
}

/// ミラーへ送るヘッダ（本線と同じ加工。B-394）。
fn mirror_headers(
    ctx: &ConnCtx,
    f: &Forward,
    headers: &[(Bytes, Bytes)],
    chunked: bool,
) -> Vec<(Bytes, Bytes)> {
    super::l7::upstream_request_headers(ctx, f, headers, 1, false, chunked).0
}

/// ミラーへ送る要求のヘッダ部（ホップバイホップを除く。chunked の本文はそのまま送るので Transfer-Encoding は残す）。
fn mirror_head(
    method: &[u8],
    path: &[u8],
    authority: &[u8],
    headers: &[(Bytes, Bytes)],
    suffix: bool,
    chunked: bool,
) -> Bytes {
    let mut head = BytesMut::with_capacity(512);
    head.put_slice(method);
    head.put_u8(b' ');
    head.put_slice(path);
    head.put_slice(b" HTTP/1.1\r\nhost: ");
    let host = authority.split(|&b| b == b':').next().unwrap_or(authority);
    if suffix {
        head.put_slice(host);
        head.put_slice(b"-shadow\r\n");
    } else {
        head.put_slice(authority);
        head.put_slice(b"\r\n");
    }
    for (k, v) in headers {
        if k.as_ref() == b"host" || k.starts_with(b":") {
            continue;
        }
        if super::httpcore::is_hop_by_hop(k) && !(chunked && k.as_ref() == b"transfer-encoding") {
            continue;
        }
        head.put_slice(k);
        head.put_slice(b": ");
        head.put_slice(v);
        head.put_slice(b"\r\n");
    }
    head.put_slice(b"connection: close\r\n\r\n");
    head.freeze()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::model::*;

    /// B-394: ミラーの要求は本線と同じく、偽の XFCC・`forward = false` のトークン・ルートが消すヘッダを送らない
    #[test]
    fn mirror_sends_sanitized_headers() {
        let cluster = Arc::new(ClusterSlot::new("shadow"));
        let mut route_ops = HeaderOps::default();
        route_ops.remove.push(Bytes::from_static(b"x-internal"));
        let action = Arc::new(ForwardAction {
            target: ClusterTarget::Single(cluster.clone()),
            timeout: None,
            idle_timeout: None,
            retry: None,
            prefix_rewrite: None,
            regex_rewrite: None,
            host_rewrite: None,
            auto_host_rewrite: false,
            mirrors: vec![(cluster.clone(), 1_000_000, true)],
            hash: Vec::new(),
            websocket: None,
            connect_terminate: false,
            cluster_not_found: 503,
        });
        let mut table = RouteTable::empty("t");
        table.vhosts.push(VirtualHost {
            name: "*".into(),
            routes: vec![MeshRoute {
                name: "r".into(),
                path: PathMatch::Any,
                case_sensitive: true,
                headers: Vec::new(),
                query: Vec::new(),
                fraction: None,
                grpc_only: false,
                action: RouteAction::Forward(action.clone()),
                request_ops: route_ops,
                response_ops: HeaderOps::default(),
                operation: Bytes::new(),
                fault: None,
                cors: None,
                rbac: None,
            }],
            request_ops: HeaderOps::default(),
            response_ops: HeaderOps::default(),
            include_attempt_count: false,
            retry: None,
            cors: None,
            fault: None,
        });
        table.catch_all = Some(0);
        let table = Arc::new(table);
        let hcm = Arc::new(HttpCm {
            stat_prefix: "gw".into(),
            routes: RouteRef::Inline(table.clone()),
            codec: Codec::Auto,
            server_name: Bytes::new(),
            xfcc: XfccMode::Sanitize,
            xfcc_subject: false,
            xfcc_uri: false,
            xfcc_dns: false,
            generate_request_id: false,
            use_remote_address: false,
            xff_num_trusted_hops: 0,
            path_normalization: Default::default(),
            websocket: false,
            filters: Vec::new(),
            peer_metadata: false,
            istio_alpn: false,
            direction: Direction::Outbound,
            stream_idle_timeout: None,
            trace_sampling: 0,
            access_logs: Vec::new(),
            tracer: None,
            linkerd: false,
        });
        let ctx = ConnCtx {
            hcm,
            direction: Direction::Outbound,
            orig_dst: None,
            peer: "10.0.0.9:5555".parse().unwrap(),
            tls: false,
            peer_principal: None,
            peer_cert_hash: None,
            peer_dns: Vec::new(),
            sni: None,
            relookup: None,
            table_version: 0,
            stats: false,
        };
        let f = Forward {
            table,
            vh: 0,
            route: 0,
            action,
            cluster,
            weighted: None,
            hash: None,
            delay: None,
            cors: None,
            jwt_remove: vec![Bytes::from_static(b"x-token")],
            jwt_add: Vec::new(),
            span: None,
            #[cfg(feature = "wasm")]
            wasm: Vec::new(),
        };
        let h = |n: &'static [u8], v: &'static [u8]| (Bytes::from_static(n), Bytes::from_static(v));
        let raw = [
            h(
                b"x-forwarded-client-cert",
                b"By=x;URI=spiffe://cluster.local/ns/prod/sa/admin",
            ),
            h(b"x-token", b"secret"),
            h(b"x-internal", b"1"),
            h(b"accept", b"*/*"),
        ];
        let head = mirror_head(
            b"GET",
            b"/a",
            b"svc:80",
            &mirror_headers(&ctx, &f, &raw, false),
            true,
            false,
        );
        let head = std::str::from_utf8(&head).unwrap().to_ascii_lowercase();
        assert!(
            head.starts_with("get /a http/1.1\r\nhost: svc-shadow\r\n"),
            "{head}"
        );
        assert!(head.contains("accept: */*\r\n"), "{head}");
        assert!(!head.contains("x-forwarded-client-cert"), "{head}");
        assert!(!head.contains("x-token"), "{head}");
        assert!(!head.contains("x-internal"), "{head}");
        assert!(head.ends_with("connection: close\r\n\r\n"), "{head}");
    }
}
