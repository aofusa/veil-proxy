// F-270
//! Compression of handler responses with the route's `[route.compression]` (HTTP/1.1, HTTP/2 and HTTP/3; buffered
//! and streaming bodies).
//!
//! The protocol paths pass the route's resolved configuration (`Backend::Handler`, `None` when compression is off
//! or the route opted out) to `dispatch::call`, which calls [`apply`] after the handler returned. A response that
//! is not compressed costs one header lookup.

use super::response::Response;
use crate::config::{AcceptedEncoding, CompressionConfig};
use bytes::Bytes;

/// The compression `[api] compression = true` gives handler routes without their own `[route.compression]`.
pub(crate) fn library_default_table() -> toml::Value {
    let mut t = toml::map::Map::new();
    t.insert("enabled".into(), toml::Value::Boolean(true));
    t.insert(
        "preferred_encodings".into(),
        toml::Value::Array(vec!["br".into(), "gzip".into()]),
    );
    t.insert("min_size".into(), toml::Value::Integer(256));
    t.insert(
        "compressible_types".into(),
        toml::Value::Array(vec![
            "application/json".into(),
            "text/".into(),
            "application/javascript".into(),
            "image/svg+xml".into(),
        ]),
    );
    toml::Value::Table(t)
}

/// Gives every handler route without `compression` the library default (`[api] compression = true`).
pub(crate) fn apply_library_default(root: &mut toml::Value) {
    let Some(routes) = root.get_mut("route").and_then(|r| r.as_array_mut()) else {
        return;
    };
    for r in routes {
        let is_handler = r
            .get("action")
            .and_then(|a| a.get("type"))
            .and_then(|t| t.as_str())
            == Some("Handler");
        if let Some(t) = r.as_table_mut() {
            if is_handler && !t.contains_key("compression") {
                t.insert("compression".into(), library_default_table());
            }
        }
    }
}

/// The q-value the `Accept-Encoding` value gives `coding` (an explicit entry, else `*`); `None` when it is not
/// acceptable (absent or `q=0`).
fn accepted_q(accept: &[u8], coding: &[u8]) -> Option<u16> {
    let mut star = None;
    for item in accept.split(|&b| b == b',') {
        let mut parts = item.split(|&b| b == b';');
        let name = trim(parts.next().unwrap_or(b""));
        let mut q = 1000u16;
        for p in parts {
            // `q=0.5`（`q = 0.5` のような空白も受ける）
            if let Some(eq) = p.iter().position(|&b| b == b'=') {
                if trim(&p[..eq]).eq_ignore_ascii_case(b"q") {
                    q = parse_q(trim(&p[eq + 1..]));
                }
            }
        }
        if name.eq_ignore_ascii_case(coding) {
            return (q > 0).then_some(q);
        }
        if name == b"*" {
            star = Some(q);
        }
    }
    star.filter(|&q| q > 0)
}

fn trim(s: &[u8]) -> &[u8] {
    let s = match s.iter().position(|b| !b.is_ascii_whitespace()) {
        Some(i) => &s[i..],
        None => return &[],
    };
    match s.iter().rposition(|b| !b.is_ascii_whitespace()) {
        Some(i) => &s[..=i],
        None => &[],
    }
}

/// `q` in thousandths (RFC 9110 section 12.4.2: at most three decimals); unparsable values count as 0.
fn parse_q(v: &[u8]) -> u16 {
    match v {
        [b'1', rest @ ..] if rest.is_empty() || rest[0] == b'.' => 1000,
        [b'0'] => 0,
        [b'0', b'.', frac @ ..] if frac.len() <= 3 && frac.iter().all(|b| b.is_ascii_digit()) => {
            let mut q = 0u16;
            for i in 0..3 {
                q = q * 10 + frac.get(i).map(|d| (d - b'0') as u16).unwrap_or(0);
            }
            q
        }
        _ => 0,
    }
}

/// Picks the coding: the highest q-value among `preferred` (ties go to the earlier one in `preferred`).
pub(crate) fn negotiate(accept: &[u8], preferred: &[String]) -> Option<AcceptedEncoding> {
    let mut best: Option<(AcceptedEncoding, u16)> = None;
    for p in preferred {
        let enc = match p.as_str() {
            "br" => AcceptedEncoding::Brotli,
            "gzip" => AcceptedEncoding::Gzip,
            "zstd" => AcceptedEncoding::Zstd,
            "deflate" => AcceptedEncoding::Deflate,
            _ => continue,
        };
        if !available(enc) {
            continue;
        }
        if let Some(q) = accepted_q(accept, p.as_bytes()) {
            if best.map(|(_, b)| q > b).unwrap_or(true) {
                best = Some((enc, q));
            }
        }
    }
    best.map(|(e, _)| e)
}

/// Whether this build can produce `enc`.
fn available(enc: AcceptedEncoding) -> bool {
    cfg!(feature = "compression") && enc != AcceptedEncoding::Identity
}

fn header<'a>(headers: &'a [(Bytes, Bytes)], name: &[u8]) -> Option<&'a Bytes> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v)
}

/// The `Accept-Encoding` of the request, kept before the request moves into the handler.
pub(crate) fn accept_encoding(headers: &[(Bytes, Bytes)]) -> Option<Bytes> {
    header(headers, b"accept-encoding").cloned()
}

/// Whether the response may be compressed at all (status, opt-out, `Content-Encoding`, `Cache-Control:
/// no-transform`, content type and, for buffered bodies, `min_size`).
fn eligible(resp: &Response, cfg: &CompressionConfig) -> bool {
    let s = resp.status;
    if resp.no_compress || s < 200 || s == 204 || s == 206 || s == 304 || resp.upgrade.is_some() {
        return false;
    }
    if resp.stream.is_none() && resp.body.len() < cfg.min_size.max(1) {
        return false;
    }
    let mut ct: Option<&[u8]> = None;
    for (n, v) in &resp.headers {
        if n.eq_ignore_ascii_case(b"content-encoding") {
            return false;
        }
        if n.eq_ignore_ascii_case(b"cache-control")
            && v.windows(12)
                .any(|w| w.eq_ignore_ascii_case(b"no-transform"))
        {
            return false;
        }
        if n.eq_ignore_ascii_case(b"content-range") {
            return false;
        }
        if n.eq_ignore_ascii_case(b"content-type") {
            ct = Some(v);
        }
    }
    let Some(ct) = ct else { return false };
    let lower = |p: &str| ct.len() >= p.len() && ct[..p.len()].eq_ignore_ascii_case(p.as_bytes());
    if cfg.skip_types.iter().any(|t| lower(t)) {
        return false;
    }
    // SSE は断片ごとの flush が要り、プロキシ・ブラウザの扱いが揃わないので、`compressible_types` に
    // 明示したときだけ圧縮する（既定の "text/" では圧縮しない）
    if lower("text/event-stream")
        && !cfg
            .compressible_types
            .iter()
            .any(|t| t.len() >= 17 && t[..17].eq_ignore_ascii_case("text/event-stream"))
    {
        return false;
    }
    cfg.compressible_types.iter().any(|t| lower(t))
}

/// Compresses the response in place when the request accepts a coding of the route's configuration.
pub(crate) fn apply(resp: &mut Response, accept: Option<&[u8]>, cfg: &CompressionConfig) {
    let Some(accept) = accept else { return };
    if !eligible(resp, cfg) {
        return;
    }
    let Some(enc) = negotiate(accept, &cfg.preferred_encodings) else {
        // 受け付ける方式が無くても、表現は Accept-Encoding で変わり得る
        add_vary(resp);
        return;
    };
    match resp.stream.take() {
        Some(s) => {
            resp.stream = Some(imp::compress_stream(s, enc, cfg));
        }
        None => match imp::compress(&resp.body, enc, cfg) {
            Some(b) => resp.body = b,
            None => return,
        },
    }
    encoded_headers(resp, enc);
}

/// For HEAD: the headers GET would get (`content-encoding`, `Vary`, weak `ETag`) without compressing the body,
/// which HEAD drops. `content-length` is removed, since the compressed length is not known without compressing.
/// Returns whether a coding was chosen.
pub(crate) fn apply_head(
    resp: &mut Response,
    accept: Option<&[u8]>,
    cfg: &CompressionConfig,
) -> bool {
    let Some(accept) = accept else { return false };
    if !eligible(resp, cfg) {
        return false;
    }
    match negotiate(accept, &cfg.preferred_encodings) {
        Some(enc) => {
            encoded_headers(resp, enc);
            true
        }
        None => {
            add_vary(resp);
            false
        }
    }
}

fn encoded_headers(resp: &mut Response, enc: crate::config::AcceptedEncoding) {
    resp.headers
        .retain(|(n, _)| !n.eq_ignore_ascii_case(b"content-length"));
    resp.headers.push((
        Bytes::from_static(b"content-encoding"),
        Bytes::from_static(enc.as_header_value()),
    ));
    add_vary(resp);
    // 強い ETag は表現（バイト列）ごとなので、圧縮した表現には弱い ETag を付ける（RFC 9110 8.8.3）
    for (n, v) in resp.headers.iter_mut() {
        if n.eq_ignore_ascii_case(b"etag") && !v.starts_with(b"W/") {
            let mut w = Vec::with_capacity(v.len() + 2);
            w.extend_from_slice(b"W/");
            w.extend_from_slice(v);
            *v = Bytes::from(w);
        }
    }
}

fn add_vary(resp: &mut Response) {
    for (n, v) in resp.headers.iter_mut() {
        if n.eq_ignore_ascii_case(b"vary") {
            if v.split(|&b| b == b',')
                .any(|t| trim(t).eq_ignore_ascii_case(b"accept-encoding") || trim(t) == b"*")
            {
                return;
            }
            let mut w = Vec::with_capacity(v.len() + 17);
            w.extend_from_slice(v);
            w.extend_from_slice(b", accept-encoding");
            *v = Bytes::from(w);
            return;
        }
    }
    resp.headers.push((
        Bytes::from_static(b"vary"),
        Bytes::from_static(b"accept-encoding"),
    ));
}

#[cfg(feature = "compression")]
mod imp {
    use super::super::stream::{BodyChunk, BodyStream};
    use crate::config::{AcceptedEncoding, CompressionConfig};
    use bytes::Bytes;
    use std::cell::RefCell;
    use std::io::Write;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    thread_local! {
        // deflate の作業領域（数百 KiB）を応答ごとに確保しない。レベルが変われば作り直す
        static DEFLATE: RefCell<Option<(u32, bool, flate2::Compress)>> = const { RefCell::new(None) };
        static ZSTD: RefCell<Option<zstd::bulk::Compressor<'static>>> = const { RefCell::new(None) };
    }

    /// Raw deflate (`zlib = false`) or zlib-wrapped deflate (the `deflate` content coding) of `input`, appended to `out`.
    fn deflate_into(input: &[u8], level: u32, zlib: bool, out: &mut Vec<u8>) -> Option<()> {
        DEFLATE.with(|cell| {
            let mut slot = cell.borrow_mut();
            let fresh = !matches!(&*slot, Some((l, z, _)) if *l == level && *z == zlib);
            if fresh {
                *slot = Some((
                    level,
                    zlib,
                    flate2::Compress::new(flate2::Compression::new(level), zlib),
                ));
            }
            let c = &mut slot.as_mut()?.2;
            c.reset();
            let start_in = c.total_in();
            loop {
                if out.capacity() - out.len() < 64 {
                    out.reserve(out.capacity().max(256));
                }
                let consumed = (c.total_in() - start_in) as usize;
                let st = c
                    .compress_vec(&input[consumed..], out, flate2::FlushCompress::Finish)
                    .ok()?;
                if st == flate2::Status::StreamEnd {
                    return Some(());
                }
            }
        })
    }

    /// One-shot compression of a buffered body (`None` if it failed; the body is then sent as is).
    pub(super) fn compress(
        body: &[u8],
        enc: AcceptedEncoding,
        cfg: &CompressionConfig,
    ) -> Option<Bytes> {
        let level = cfg.gzip_level.clamp(1, 9);
        match enc {
            AcceptedEncoding::Gzip => {
                let mut out = Vec::with_capacity(body.len() / 2 + 64);
                out.extend_from_slice(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff]);
                deflate_into(body, level, false, &mut out)?;
                let mut crc = flate2::Crc::new();
                crc.update(body);
                out.extend_from_slice(&crc.sum().to_le_bytes());
                out.extend_from_slice(&(body.len() as u32).to_le_bytes());
                Some(Bytes::from(out))
            }
            AcceptedEncoding::Deflate => {
                let mut out = Vec::with_capacity(body.len() / 2 + 64);
                deflate_into(body, level, true, &mut out)?;
                Some(Bytes::from(out))
            }
            AcceptedEncoding::Brotli => {
                let mut out = Vec::with_capacity(body.len() / 2 + 64);
                let params = brotli::enc::BrotliEncoderParams {
                    quality: cfg.brotli_level.min(11) as i32,
                    size_hint: body.len(),
                    ..Default::default()
                };
                brotli::BrotliCompress(&mut &body[..], &mut out, &params).ok()?;
                Some(Bytes::from(out))
            }
            AcceptedEncoding::Zstd => ZSTD.with(|cell| {
                let mut slot = cell.borrow_mut();
                let c = match slot.as_mut() {
                    Some(c) => c,
                    None => slot.get_or_insert(zstd::bulk::Compressor::new(cfg.zstd_level).ok()?),
                };
                c.set_compression_level(cfg.zstd_level).ok()?;
                c.compress(body).ok().map(Bytes::from)
            }),
            AcceptedEncoding::Identity => None,
        }
    }

    enum Enc {
        Gzip(flate2::write::GzEncoder<Vec<u8>>),
        Deflate(flate2::write::ZlibEncoder<Vec<u8>>),
        Brotli(Box<brotli::CompressorWriter<Vec<u8>>>),
        Zstd(zstd::stream::write::Encoder<'static, Vec<u8>>),
    }

    impl Enc {
        fn new(enc: AcceptedEncoding, cfg: &CompressionConfig) -> Option<Enc> {
            let gz = flate2::Compression::new(cfg.gzip_level.clamp(1, 9));
            Some(match enc {
                AcceptedEncoding::Gzip => Enc::Gzip(flate2::write::GzEncoder::new(Vec::new(), gz)),
                AcceptedEncoding::Deflate => {
                    Enc::Deflate(flate2::write::ZlibEncoder::new(Vec::new(), gz))
                }
                AcceptedEncoding::Brotli => Enc::Brotli(Box::new(brotli::CompressorWriter::new(
                    Vec::new(),
                    4096,
                    cfg.brotli_level.min(11),
                    22,
                ))),
                AcceptedEncoding::Zstd => {
                    Enc::Zstd(zstd::stream::write::Encoder::new(Vec::new(), cfg.zstd_level).ok()?)
                }
                AcceptedEncoding::Identity => return None,
            })
        }

        /// Compresses `data` and flushes, so that the chunk reaches the client now (SSE, progress output).
        fn chunk(&mut self, data: &[u8]) -> std::io::Result<Bytes> {
            fn take(v: &mut Vec<u8>) -> Bytes {
                Bytes::from(std::mem::take(v))
            }
            Ok(match self {
                Enc::Gzip(e) => {
                    e.write_all(data)?;
                    e.flush()?;
                    take(e.get_mut())
                }
                Enc::Deflate(e) => {
                    e.write_all(data)?;
                    e.flush()?;
                    take(e.get_mut())
                }
                Enc::Brotli(e) => {
                    e.write_all(data)?;
                    e.flush()?;
                    take(e.get_mut())
                }
                Enc::Zstd(e) => {
                    e.write_all(data)?;
                    e.flush()?;
                    take(e.get_mut())
                }
            })
        }

        fn finish(self) -> std::io::Result<Bytes> {
            Ok(Bytes::from(match self {
                Enc::Gzip(e) => e.finish()?,
                Enc::Deflate(e) => e.finish()?,
                Enc::Brotli(e) => e.into_inner(),
                Enc::Zstd(e) => e.finish()?,
            }))
        }
    }

    struct Compressing {
        inner: BodyStream,
        enc: Option<Enc>,
        pending: Option<BodyChunk>,
    }

    impl futures::Stream for Compressing {
        type Item = BodyChunk;

        fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<BodyChunk>> {
            let this = &mut *self;
            if let Some(c) = this.pending.take() {
                return Poll::Ready(Some(c));
            }
            loop {
                let Some(enc) = this.enc.as_mut() else {
                    return Poll::Ready(None);
                };
                let item = match this.inner.as_mut().poll_next(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(i) => i,
                };
                match item {
                    Some(BodyChunk::Data(d)) => match enc.chunk(&d) {
                        Ok(b) if b.is_empty() => continue,
                        Ok(b) => return Poll::Ready(Some(BodyChunk::Data(b))),
                        Err(e) => {
                            this.enc = None;
                            return Poll::Ready(Some(BodyChunk::Abort(e.to_string())));
                        }
                    },
                    Some(BodyChunk::Abort(r)) => {
                        this.enc = None;
                        return Poll::Ready(Some(BodyChunk::Abort(r)));
                    }
                    end => {
                        // 終端・トレーラ: 圧縮の終わりを先に送る
                        let enc = this.enc.take().expect("checked above");
                        this.pending = end;
                        return match enc.finish() {
                            Ok(b) if b.is_empty() => Poll::Ready(this.pending.take()),
                            Ok(b) => Poll::Ready(Some(BodyChunk::Data(b))),
                            Err(e) => {
                                this.pending = None;
                                Poll::Ready(Some(BodyChunk::Abort(e.to_string())))
                            }
                        };
                    }
                }
            }
        }
    }

    /// Wraps a streaming body so that each chunk is compressed and flushed.
    pub(super) fn compress_stream(
        s: BodyStream,
        enc: AcceptedEncoding,
        cfg: &CompressionConfig,
    ) -> BodyStream {
        match Enc::new(enc, cfg) {
            Some(e) => Box::pin(futures::StreamExt::fuse(Compressing {
                inner: s,
                enc: Some(e),
                pending: None,
            })),
            None => s,
        }
    }
}

#[cfg(not(feature = "compression"))]
mod imp {
    // `available` が false なので `negotiate` は何も選ばず、ここには来ない
    use super::super::stream::BodyStream;
    use crate::config::{AcceptedEncoding, CompressionConfig};
    use bytes::Bytes;

    pub(super) fn compress(_: &[u8], _: AcceptedEncoding, _: &CompressionConfig) -> Option<Bytes> {
        None
    }

    pub(super) fn compress_stream(
        s: BodyStream,
        _: AcceptedEncoding,
        _: &CompressionConfig,
    ) -> BodyStream {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> CompressionConfig {
        CompressionConfig {
            enabled: true,
            preferred_encodings: vec!["br".into(), "gzip".into()],
            min_size: 16,
            ..Default::default()
        }
    }

    #[test]
    fn accept_encoding_q_values() {
        let p: Vec<String> = vec!["br".into(), "gzip".into()];
        let n = |a: &str| negotiate(a.as_bytes(), &p);
        if !cfg!(feature = "compression") {
            assert_eq!(n("gzip, br"), None);
            return;
        }
        assert_eq!(n("gzip, br"), Some(AcceptedEncoding::Brotli));
        assert_eq!(n("gzip"), Some(AcceptedEncoding::Gzip));
        assert_eq!(n("GZIP;q=0.5, br;q=0.4"), Some(AcceptedEncoding::Gzip));
        assert_eq!(n("br;q=0, gzip"), Some(AcceptedEncoding::Gzip));
        assert_eq!(n("br;q=0, gzip;q=0"), None);
        assert_eq!(n("*"), Some(AcceptedEncoding::Brotli));
        assert_eq!(n("*;q=0.1, br;q=0"), Some(AcceptedEncoding::Gzip));
        assert_eq!(n("identity"), None);
        assert_eq!(n("identity;q=0"), None);
        assert_eq!(n(""), None);
        assert_eq!(n("deflate, zstd"), None);
        assert_eq!(
            n("gzip ; q=1.0 , br ; q = 0.9"),
            Some(AcceptedEncoding::Gzip)
        );
        assert_eq!(parse_q(b"0.001"), 1);
        assert_eq!(parse_q(b"0.5"), 500);
        assert_eq!(parse_q(b"1.000"), 1000);
        assert_eq!(parse_q(b"x"), 0);
    }

    fn json_resp(n: usize) -> Response {
        Response::json_bytes(Bytes::from(format!("{{\"a\":\"{}\"}}", "x".repeat(n))))
    }

    #[test]
    fn head_gets_the_get_headers_without_compressing() {
        let c = cfg();
        let mut r = json_resp(100).with_header("content-length", "108");
        let body = r.body().clone();
        let chosen = apply_head(&mut r, Some(b"gzip"), &c);
        assert_eq!(r.body(), &body);
        assert_eq!(r.header("vary"), Some(&b"accept-encoding"[..]));
        if cfg!(feature = "compression") {
            assert!(chosen);
            assert_eq!(r.header("content-encoding"), Some(&b"gzip"[..]));
            assert!(r.header("content-length").is_none());
        } else {
            assert!(!chosen);
        }
        let mut r = json_resp(4);
        assert!(!apply_head(&mut r, Some(b"gzip"), &c), "below min_size");
    }

    #[test]
    fn conditions_that_skip_compression() {
        let c = cfg();
        let gz = Some(&b"gzip"[..]);
        let mut r = json_resp(4);
        apply(&mut r, gz, &c);
        assert!(r.header("content-encoding").is_none(), "below min_size");
        let mut r = json_resp(100);
        apply(&mut r, None, &c);
        assert!(r.header("content-encoding").is_none(), "no Accept-Encoding");
        assert!(r.header("vary").is_none());
        let mut r = json_resp(100).with_header("content-encoding", "gzip");
        let before = r.body().clone();
        apply(&mut r, gz, &c);
        assert_eq!(r.body(), &before, "already encoded");
        let mut r = json_resp(100).with_header("cache-control", "public, No-Transform");
        apply(&mut r, gz, &c);
        assert!(r.header("content-encoding").is_none(), "no-transform");
        let mut r = json_resp(100).with_status(204);
        apply(&mut r, gz, &c);
        assert!(r.header("content-encoding").is_none(), "204");
        let mut r = json_resp(100).with_status(206);
        apply(&mut r, gz, &c);
        assert!(r.header("content-encoding").is_none(), "206");
        let mut r = Response::bytes(vec![b'x'; 100]);
        apply(&mut r, gz, &c);
        assert!(
            r.header("content-encoding").is_none(),
            "octet-stream is skipped"
        );
        let mut r = json_resp(100).no_compression();
        apply(&mut r, gz, &c);
        assert!(r.header("content-encoding").is_none(), "opted out");
        let mut r = Response::stream(200, futures::stream::empty())
            .with_header("content-type", "text/event-stream");
        apply(&mut r, gz, &c);
        assert!(r.header("content-encoding").is_none(), "SSE by default");
    }

    #[cfg(feature = "compression")]
    #[test]
    fn buffered_gzip_and_brotli_round_trip() {
        use std::io::Read;
        let c = cfg();
        let original = json_resp(500);
        let mut r = json_resp(500)
            .with_header("content-length", "999")
            .with_header("etag", "\"abc\"")
            .with_header("vary", "origin");
        apply(&mut r, Some(b"gzip"), &c);
        assert_eq!(r.header("content-encoding"), Some(&b"gzip"[..]));
        assert_eq!(r.header("vary"), Some(&b"origin, accept-encoding"[..]));
        assert_eq!(r.header("etag"), Some(&b"W/\"abc\""[..]));
        assert!(r.header("content-length").is_none());
        assert!(r.body().len() < original.body().len());
        let mut d = Vec::new();
        flate2::read::GzDecoder::new(&r.body()[..])
            .read_to_end(&mut d)
            .unwrap();
        assert_eq!(&d[..], &original.body()[..]);
        // 2 回目はスレッドローカルの作業領域を使い回す（結果は同じ）
        let mut r2 = json_resp(500);
        apply(&mut r2, Some(b"gzip"), &c);
        let mut d2 = Vec::new();
        flate2::read::GzDecoder::new(&r2.body()[..])
            .read_to_end(&mut d2)
            .unwrap();
        assert_eq!(d2, d);

        let mut r = json_resp(500);
        apply(&mut r, Some(b"gzip, br"), &c);
        assert_eq!(r.header("content-encoding"), Some(&b"br"[..]));
        let mut d = Vec::new();
        brotli::Decompressor::new(&r.body()[..], 4096)
            .read_to_end(&mut d)
            .unwrap();
        assert_eq!(&d[..], &original.body()[..]);

        let mut c3 = cfg();
        c3.preferred_encodings = vec!["zstd".into(), "deflate".into()];
        let mut r = json_resp(500);
        apply(&mut r, Some(b"zstd"), &c3);
        assert_eq!(
            zstd::decode_all(&r.body()[..]).unwrap(),
            original.body().to_vec()
        );
        let mut r = json_resp(500);
        apply(&mut r, Some(b"deflate"), &c3);
        let mut d = Vec::new();
        flate2::read::ZlibDecoder::new(&r.body()[..])
            .read_to_end(&mut d)
            .unwrap();
        assert_eq!(&d[..], &original.body()[..]);
    }

    #[cfg(feature = "compression")]
    #[test]
    fn streaming_round_trip_keeps_trailers() {
        use super::super::stream::BodyChunk;
        use futures::StreamExt;
        use std::io::Read;
        let chunks: Vec<BodyChunk> = (0..5)
            .map(|i| BodyChunk::Data(Bytes::from(format!("line {} {}\n", i, "y".repeat(40)))))
            .chain([BodyChunk::Trailers(vec![(
                Bytes::from_static(b"x-t"),
                Bytes::from_static(b"1"),
            )])])
            .collect();
        for enc in ["gzip", "br"] {
            let mut r = Response::stream(200, futures::stream::iter(chunks.clone()))
                .with_header("content-type", "text/plain");
            apply(&mut r, Some(enc.as_bytes()), &cfg());
            assert_eq!(r.header("content-encoding"), Some(enc.as_bytes()));
            let mut s = r.take_stream().unwrap();
            let (mut data, mut trailers, mut n) = (Vec::new(), None, 0);
            futures::executor::block_on(async {
                while let Some(c) = s.next().await {
                    match c {
                        BodyChunk::Data(b) => {
                            n += 1;
                            data.extend_from_slice(&b)
                        }
                        BodyChunk::Trailers(t) => trailers = Some(t),
                        BodyChunk::Abort(e) => panic!("{}", e),
                    }
                }
            });
            assert!(n >= 5, "each chunk is flushed ({})", n);
            assert_eq!(trailers.unwrap()[0].1, Bytes::from_static(b"1"));
            let mut out = Vec::new();
            if enc == "gzip" {
                flate2::read::GzDecoder::new(&data[..])
                    .read_to_end(&mut out)
                    .unwrap();
            } else {
                brotli::Decompressor::new(&data[..], 4096)
                    .read_to_end(&mut out)
                    .unwrap();
            }
            let want: Vec<u8> = (0..5)
                .flat_map(|i| format!("line {} {}\n", i, "y".repeat(40)).into_bytes())
                .collect();
            assert_eq!(out, want);
        }
    }

    #[test]
    fn library_default_goes_only_to_handler_routes_without_compression() {
        let mut root: toml::Value = toml::from_str(
            r#"
            [[route]]
            action = { type = "Handler", handler = "a" }
            [[route]]
            action = { type = "Handler", handler = "b" }
            compression = { enabled = false }
            [[route]]
            action = { type = "Proxy", url = "http://x" }
            "#,
        )
        .unwrap();
        apply_library_default(&mut root);
        let r = root["route"].as_array().unwrap();
        assert_eq!(r[0]["compression"]["enabled"].as_bool(), Some(true));
        assert_eq!(r[1]["compression"]["enabled"].as_bool(), Some(false));
        assert!(r[2].get("compression").is_none());
        let c: CompressionConfig = r[0]["compression"].clone().try_into().unwrap();
        assert_eq!(c.min_size, 256);
        assert_eq!(c.preferred_encodings, vec!["br", "gzip"]);
    }
}
