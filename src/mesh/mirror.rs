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
    for (slot, fraction, suffix) in &f.action.mirrors {
        if *fraction < 1_000_000 && super::httpcore::rand_u32() % 1_000_000 >= *fraction {
            continue;
        }
        let Ok(picked) = upstream::pick(slot, ctx.orig_dst, None, &[]) else {
            continue;
        };
        // ヘッダ（ホップバイホップを除く。chunked の本文はそのまま送るので Transfer-Encoding は残す）
        let mut head = BytesMut::with_capacity(512);
        head.put_slice(method);
        head.put_u8(b' ');
        head.put_slice(path);
        head.put_slice(b" HTTP/1.1\r\nhost: ");
        let host = authority.split(|&b| b == b':').next().unwrap_or(authority);
        if *suffix {
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
            if super::httpcore::is_hop_by_hop(k) && !(chunked && k.as_ref() == b"transfer-encoding")
            {
                continue;
            }
            head.put_slice(k);
            head.put_slice(b": ");
            head.put_slice(v);
            head.put_slice(b"\r\n");
        }
        head.put_slice(b"connection: close\r\n\r\n");
        let head = head.freeze();
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
