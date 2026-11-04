//! 静的配信（`type = "File"`・`mode = "memory"`）の応答の決め方（HTTP/1.1・HTTP/2・HTTP/3 で共有）
//!
//! 以前は 3 つのプロトコルがそれぞれ応答を組み立てており、HEAD に本文を付ける（B-266）・HTTP/1.1 だけ圧縮しない
//! （B-269）・HTTP/1.1 だけ Range に応える（F-286）・どこも `ETag` を返さない（F-287）と、同じ設定で挙動が違った。
//! ここでは「どの応答を返すか」（[`plan`]）と「どのヘッダを付けるか」（[`emit_headers`]）だけを決め、
//! 送り方（`sendfile`・HTTP/2 の DATA・HTTP/3 のストリーム）は各プロトコルに任せる。
//!
//! 要求ごとの経路は確保しない: 検証子（`ETag`・`Last-Modified`）はファイル情報のキャッシュを作るときに
//! 1 回だけ組み立てた `Bytes` を参照カウントで渡し、要求ヘッダは借用のまま見る。

use bytes::Bytes;
use std::time::SystemTime;

use crate::config::{AcceptedEncoding, CompressionConfig};

/// 応答の検証子（RFC 9110 §8.8）。ファイル情報・memory の内容を作るときに 1 回だけ組み立てる。
#[derive(Clone, Debug)]
pub struct Validators {
    /// 強い ETag（引用符付き。nginx と同じ `"<mtime の 16 進>-<大きさの 16 進>"`）
    pub etag: Bytes,
    /// 圧縮した表現の弱い ETag（`W/"…"`。表現が違うので強い ETag は使えない）
    pub weak_etag: Bytes,
    /// `Last-Modified`（IMF-fixdate）。更新時刻が分からなければ空
    pub last_modified: Bytes,
    /// 更新時刻（UNIX 秒）。`If-Modified-Since` の比較に使う
    pub mtime_secs: Option<u64>,
    /// `content-type` の値（HTTP/2・HTTP/3 で要求ごとに複製しない）
    pub content_type: Bytes,
    /// 全体の長さの 10 進（200 の `content-length`。同上）
    pub full_length: Bytes,
}

impl Validators {
    /// ファイルの大きさと更新時刻から作る。
    pub fn for_file(size: u64, modified: Option<SystemTime>, mime: &str) -> Self {
        let mtime_secs = modified
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        let tag = format!("{:x}-{:x}", mtime_secs.unwrap_or(0), size);
        Self::build(&tag, mtime_secs, mime, size)
    }

    /// ファイル情報のキャッシュに載せる形（`Arc`）で作る。
    pub fn shared_for_file(
        size: u64,
        modified: Option<SystemTime>,
        mime: &str,
    ) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::for_file(size, modified, mime))
    }

    /// memory の内容から作る（設定のロード時に 1 回）。内容のハッシュを ETag にする（再読み込みで内容が
    /// 変わらなければ同じ ETag のまま）。
    pub fn for_memory(body: &[u8], modified: Option<SystemTime>, mime: &str) -> Self {
        let mtime_secs = modified
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in body {
            h ^= b as u64;
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        let tag = format!("m{:x}-{:x}", h, body.len());
        Self::build(&tag, mtime_secs, mime, body.len() as u64)
    }

    fn build(tag: &str, mtime_secs: Option<u64>, mime: &str, size: u64) -> Self {
        Self {
            content_type: Bytes::copy_from_slice(mime.as_bytes()),
            full_length: Bytes::copy_from_slice(itoa::Buffer::new().format(size).as_bytes()),
            etag: Bytes::from(format!("\"{tag}\"")),
            weak_etag: Bytes::from(format!("W/\"{tag}\"")),
            last_modified: mtime_secs
                .map(|s| Bytes::from(http_date(s)))
                .unwrap_or_default(),
            mtime_secs,
        }
    }
}

/// `mode = "memory"` の内容（設定のロード時に読む。B-266・F-286・F-287）
pub struct MemoryContent {
    pub body: Bytes,
    pub validators: Validators,
    /// 圧縮した本体（方式ごとに初回に作る。ルートの圧縮設定はこの内容と一緒にロードし直すので変わらない）
    compressed: [std::sync::OnceLock<Bytes>; 4],
}

impl MemoryContent {
    pub fn new(data: Vec<u8>, modified: Option<SystemTime>, mime: &str) -> Self {
        let validators = Validators::for_memory(&data, modified, mime);
        Self {
            body: Bytes::from(data),
            validators,
            compressed: Default::default(),
        }
    }

    #[inline]
    pub fn len(&self) -> u64 {
        self.body.len() as u64
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.body.is_empty()
    }

    /// `encoding` で圧縮した本体（初回だけ圧縮し、以後は参照カウントだけ）
    pub fn compressed(&self, encoding: AcceptedEncoding, compression: &CompressionConfig) -> Bytes {
        let slot = match encoding {
            AcceptedEncoding::Zstd => 0,
            AcceptedEncoding::Brotli => 1,
            AcceptedEncoding::Gzip => 2,
            AcceptedEncoding::Deflate => 3,
            AcceptedEncoding::Identity => return self.body.clone(),
        };
        self.compressed[slot]
            .get_or_init(|| Bytes::from(compress_body(&self.body, encoding, compression)))
            .clone()
    }

    /// `plan` の本文（206 は切り出し、圧縮は圧縮した全体。304・416 は空）
    pub fn body_for(&self, plan: &Plan, compression: &CompressionConfig) -> Bytes {
        match *plan {
            Plan::Full {
                encoding: Some(enc),
            } => self.compressed(enc, compression),
            Plan::Full { encoding: None } => self.body.clone(),
            Plan::Partial { start, end } => self.body.slice(start as usize..=end as usize),
            Plan::NotModified | Plan::Unsatisfiable => Bytes::new(),
        }
    }
}

/// 静的配信が見る要求ヘッダ（借用。走査は 1 回）
#[derive(Clone, Copy, Debug)]
pub struct StaticRequest<'a> {
    /// HEAD（本文を送らない）
    pub head: bool,
    pub range: Option<&'a [u8]>,
    pub if_range: Option<&'a [u8]>,
    pub if_none_match: Option<&'a [u8]>,
    pub if_modified_since: Option<&'a [u8]>,
    /// `Accept-Encoding` の生の値（圧縮が有効なルートでだけ解釈する。解釈は小文字化で確保するため）
    pub accept_encoding: Option<&'a [u8]>,
}

impl<'a> StaticRequest<'a> {
    /// 要求のメソッドとヘッダから作る。
    pub fn from_headers<I>(method: &[u8], headers: I) -> Self
    where
        I: IntoIterator<Item = (&'a [u8], &'a [u8])>,
    {
        let mut r = StaticRequest {
            head: method.eq_ignore_ascii_case(b"HEAD"),
            range: None,
            if_range: None,
            if_none_match: None,
            if_modified_since: None,
            accept_encoding: None,
        };
        for (n, v) in headers {
            // 名前の長さで先に振り分ける（ほとんどのヘッダは比較 1 回で外れる）
            match n.len() {
                5 if n.eq_ignore_ascii_case(b"range") => r.range = Some(v),
                8 if n.eq_ignore_ascii_case(b"if-range") => r.if_range = Some(v),
                13 if n.eq_ignore_ascii_case(b"if-none-match") => r.if_none_match = Some(v),
                15 if n.eq_ignore_ascii_case(b"accept-encoding") => r.accept_encoding = Some(v),
                17 if n.eq_ignore_ascii_case(b"if-modified-since") => r.if_modified_since = Some(v),
                _ => {}
            }
        }
        r
    }
}

/// 返す応答の種類
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Plan {
    /// 304（本文なし）
    NotModified,
    /// 416（`Content-Range: bytes */<size>`）
    Unsatisfiable,
    /// 206（`start..=end`）
    Partial { start: u64, end: u64 },
    /// 200。`encoding` があればその方式で圧縮した全体を返す
    Full { encoding: Option<AcceptedEncoding> },
}

impl Plan {
    /// ステータスコード
    #[inline]
    pub fn status(&self) -> u16 {
        match self {
            Plan::NotModified => 304,
            Plan::Unsatisfiable => 416,
            Plan::Partial { .. } => 206,
            Plan::Full { .. } => 200,
        }
    }
}

/// その場で圧縮する静的ファイルの大きさの上限（B-283）。圧縮には全体を読み込むので、これより大きいファイルは
/// 圧縮せず（`sendfile` のまま）送る（並行する要求ごとに大きいファイルを読み込み・圧縮させない）
pub const MAX_STATIC_COMPRESS_SIZE: u64 = 8 * 1024 * 1024;

/// 返す応答を決める（RFC 9110 §13.2.2 の順: `If-None-Match` → `If-Modified-Since` → `Range`/`If-Range`）。
///
/// 圧縮する応答には Range を当てない（nginx の gzip と同じ。表現ごとの範囲を持たない）。
pub fn plan(
    req: &StaticRequest<'_>,
    size: u64,
    mime: &str,
    validators: Option<&Validators>,
    compression: &CompressionConfig,
) -> Plan {
    if let Some(v) = validators {
        if let Some(inm) = req.if_none_match {
            if etag_list_matches_weak(inm, &v.etag) {
                return Plan::NotModified;
            }
        } else if let (Some(ims), Some(mtime)) = (req.if_modified_since, v.mtime_secs) {
            if parse_http_date(ims).is_some_and(|t| mtime <= t) {
                return Plan::NotModified;
            }
        }
    }

    let encoding = match req.accept_encoding {
        Some(ae) if compression.enabled && size <= MAX_STATIC_COMPRESS_SIZE => compression
            .should_compress(
                AcceptedEncoding::parse(ae),
                Some(mime.as_bytes()),
                usize::try_from(size).ok(),
                None,
            ),
        _ => None,
    };
    if encoding.is_some() {
        return Plan::Full { encoding };
    }

    if let Some(range) = req.range {
        if if_range_allows(req.if_range, validators) {
            if let Some(parsed) = crate::http_utils::parse_range_header(range) {
                if let Some(first) = parsed.ranges.first() {
                    return match crate::http_utils::normalize_range(first, size) {
                        Some((start, end)) => Plan::Partial { start, end },
                        None => Plan::Unsatisfiable,
                    };
                }
            }
        }
    }
    Plan::Full { encoding: None }
}

/// `If-Range` が無いか、検証子と一致するか（強い比較。日付は `Last-Modified` と完全一致）
fn if_range_allows(if_range: Option<&[u8]>, validators: Option<&Validators>) -> bool {
    let Some(ir) = if_range else {
        return true;
    };
    let Some(v) = validators else {
        return false;
    };
    let ir = trim_ows(ir);
    if ir.first() == Some(&b'"') {
        return ir == &v.etag[..];
    }
    !v.last_modified.is_empty() && ir == &v.last_modified[..]
}

/// `If-None-Match` の値のどれかが `etag` と弱い比較で一致するか（`*` は常に一致）
fn etag_list_matches_weak(list: &[u8], etag: &[u8]) -> bool {
    let opaque = |t: &[u8]| -> Option<usize> {
        // `W/` を外した引用符付きの部分の位置
        let t = trim_ows(t);
        let off = if t.len() >= 2 && t[..2].eq_ignore_ascii_case(b"W/") {
            2
        } else {
            0
        };
        (t.len() >= off + 2 && t[off] == b'"' && t[t.len() - 1] == b'"').then_some(off)
    };
    let Some(eoff) = opaque(etag) else {
        return false;
    };
    let ours = &trim_ows(etag)[eoff..];
    for item in list.split(|&b| b == b',') {
        let item = trim_ows(item);
        if item == b"*" {
            return true;
        }
        if let Some(off) = opaque(item) {
            if &item[off..] == ours {
                return true;
            }
        }
    }
    false
}

#[inline]
fn trim_ows(v: &[u8]) -> &[u8] {
    let start = v
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(v.len());
    let end = v
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |i| i + 1);
    &v[start..end]
}

/// `compression` がこの型・大きさの応答を圧縮し得るか（`Vary: Accept-Encoding` を付けるか）
#[inline]
pub fn compression_applies(compression: &CompressionConfig, mime: &str, size: u64) -> bool {
    compression.enabled
        && size <= MAX_STATIC_COMPRESS_SIZE
        && compression
            .should_compress(
                AcceptedEncoding::Zstd,
                Some(mime.as_bytes()),
                usize::try_from(size).ok(),
                None,
            )
            .is_some()
}

/// [`emit_headers`] が渡すヘッダの値（HTTP/2・HTTP/3 が `Bytes` にするときに複製を減らす）
#[derive(Clone, Copy, Debug)]
pub enum HVal<'a> {
    Static(&'static [u8]),
    /// 検証子（参照カウントだけで `Bytes` にできる）
    Shared(&'a Bytes),
    Borrowed(&'a [u8]),
}

impl HVal<'_> {
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        match self {
            HVal::Static(s) => s,
            HVal::Shared(b) => b,
            HVal::Borrowed(s) => s,
        }
    }

    #[inline]
    pub fn to_bytes(self) -> Bytes {
        match self {
            HVal::Static(s) => Bytes::from_static(s),
            HVal::Shared(b) => b.clone(),
            HVal::Borrowed(s) => Bytes::copy_from_slice(s),
        }
    }
}

/// 応答ヘッダを `push(name, value)` で渡す（名前は小文字。`Content-Length` は送り方で決まるので呼び出し側）。
///
/// 付けるもの: `content-type`（304 以外）、`accept-ranges`（圧縮しない表現）、`content-range`（206・416）、
/// `etag`・`last-modified`、`content-encoding`、`vary`（圧縮し得る型）。
pub fn emit_headers<F>(
    plan: &Plan,
    size: u64,
    mime: &str,
    validators: Option<&Validators>,
    vary: bool,
    mut push: F,
) where
    F: FnMut(&'static [u8], HVal<'_>),
{
    let encoding = match plan {
        Plan::Full { encoding } => *encoding,
        _ => None,
    };
    if !matches!(plan, Plan::NotModified) {
        match validators {
            Some(v) => push(b"content-type", HVal::Shared(&v.content_type)),
            None => push(b"content-type", HVal::Borrowed(mime.as_bytes())),
        }
    }
    match plan {
        Plan::Partial { start, end } => {
            let mut v = [0u8; CONTENT_RANGE_MAX];
            let n = write_content_range(&mut v, Some((*start, *end)), size);
            push(b"accept-ranges", HVal::Static(b"bytes"));
            push(b"content-range", HVal::Borrowed(&v[..n]));
        }
        Plan::Unsatisfiable => {
            let mut v = [0u8; CONTENT_RANGE_MAX];
            let n = write_content_range(&mut v, None, size);
            push(b"content-range", HVal::Borrowed(&v[..n]));
        }
        Plan::Full { encoding: None } => push(b"accept-ranges", HVal::Static(b"bytes")),
        _ => {}
    }
    if let Some(v) = validators {
        if !matches!(plan, Plan::Unsatisfiable) {
            if encoding.is_some() {
                push(b"etag", HVal::Shared(&v.weak_etag));
            } else {
                push(b"etag", HVal::Shared(&v.etag));
            }
            if !v.last_modified.is_empty() {
                push(b"last-modified", HVal::Shared(&v.last_modified));
            }
        }
    }
    if let Some(enc) = encoding {
        push(b"content-encoding", HVal::Static(enc.as_header_value()));
    }
    if vary && !matches!(plan, Plan::Unsatisfiable) {
        push(b"vary", HVal::Static(b"Accept-Encoding"));
    }
}

/// HTTP/2・HTTP/3 の静的配信の応答ヘッダ（小文字の名前の `Bytes`）を `out` に足す。`emit_headers` のヘッダに
/// `content-length`（304 以外）・`remove_response_headers`・`add_response_headers`・CORS をかける。
/// `server`・`alt-svc` はプロトコルごとに呼び出し側が足す。
#[allow(clippy::too_many_arguments)]
pub fn push_bytes_headers(
    out: &mut Vec<(Bytes, Bytes)>,
    plan: &Plan,
    size: u64,
    content_length: u64,
    mime: &str,
    validators: Option<&Validators>,
    vary: bool,
    security: &crate::config::SecurityConfig,
) {
    let removes = !security.remove_response_headers.is_empty();
    emit_headers(plan, size, mime, validators, vary, |n, v| {
        if !removes || !security.removes_response_header(n) {
            out.push((Bytes::from_static(n), v.to_bytes()));
        }
    });
    if !matches!(plan, Plan::NotModified) {
        let value = match validators {
            Some(v) if content_length == size => v.full_length.clone(),
            _ => Bytes::copy_from_slice(itoa::Buffer::new().format(content_length).as_bytes()),
        };
        out.push((Bytes::from_static(b"content-length"), value));
    }
    if !security.add_response_headers.is_empty() {
        out.extend(security.add_response_header_bytes().iter().cloned());
    }
    // `[route.traffic.cors]` の実際の応答のヘッダ（F-273。CORS の無いルートは分岐だけ）
    crate::route_cors::apply_bytes(out, security);
}

/// HTTP/2・HTTP/3 の静的配信の応答（ステータス・ヘッダ・本文）を決める。`data` はファイル全体（参照カウント共有）で、
/// 206 は `slice`、圧縮しないときはそのまま返す（ディープコピーしない。F-157）。
///
/// 圧縮した本体は `memory` があればそこに持たせ、`static_ctx`（静的配信のパスと静的コンテンツキャッシュの設定。F-169）が
/// あれば `cache::compressed` でキャッシュする（設定が無効なら毎回圧縮する）。`server`・`alt-svc` は呼び出し側が足す。
#[allow(clippy::too_many_arguments)]
pub fn build_bytes_response(
    sreq: &StaticRequest<'_>,
    data: Bytes,
    mime: &str,
    validators: &Validators,
    security: &crate::config::SecurityConfig,
    compression: &CompressionConfig,
    static_ctx: Option<(&std::path::Path, &crate::cache::StaticContentCacheConfig)>,
    memory: Option<&MemoryContent>,
) -> (u16, Vec<(Bytes, Bytes)>, Bytes) {
    let size = data.len() as u64;
    let plan = plan(sreq, size, mime, Some(validators), compression);
    let vary = compression_applies(compression, mime, size);
    let body = match plan {
        Plan::Full {
            encoding: Some(enc),
        } => match (memory, static_ctx) {
            (Some(m), _) => m.compressed(enc, compression),
            (None, Some((path, static_cfg))) => {
                let level = crate::cache::compressed::compression_level(enc, compression);
                crate::cache::compressed::get_or_compress(path, enc, level, static_cfg, || {
                    compress_body(&data, enc, compression)
                })
            }
            (None, None) => Bytes::from(compress_body(&data, enc, compression)),
        },
        Plan::Full { encoding: None } => data,
        Plan::Partial { start, end } => data.slice(start as usize..=end as usize),
        Plan::NotModified | Plan::Unsatisfiable => Bytes::new(),
    };
    let mut out: Vec<(Bytes, Bytes)> = Vec::with_capacity(12);
    push_bytes_headers(
        &mut out,
        &plan,
        size,
        body.len() as u64,
        mime,
        Some(validators),
        vary,
        security,
    );
    (plan.status(), out, body)
}

/// `bytes <u64>-<u64>/<u64>` の最大の長さ（6 + 20 + 1 + 20 + 1 + 20 = 68）
const CONTENT_RANGE_MAX: usize = 80;

/// `bytes <start>-<end>/<size>`（範囲が無ければ `bytes */<size>`）をスタックの配列へ書く
fn write_content_range(
    out: &mut [u8; CONTENT_RANGE_MAX],
    range: Option<(u64, u64)>,
    size: u64,
) -> usize {
    let mut n = 0usize;
    let mut put = |s: &[u8]| {
        out[n..n + s.len()].copy_from_slice(s);
        n += s.len();
    };
    let mut num = itoa::Buffer::new();
    put(b"bytes ");
    match range {
        Some((start, end)) => {
            put(num.format(start).as_bytes());
            put(b"-");
            put(num.format(end).as_bytes());
        }
        None => put(b"*"),
    }
    put(b"/");
    put(num.format(size).as_bytes());
    n
}

/// 本文を圧縮する（HTTP/1.1・HTTP/2・HTTP/3 の静的配信とプロキシで共有）。失敗したら元のまま返す。
#[cfg(feature = "compression")]
pub fn compress_body(
    body: &[u8],
    encoding: AcceptedEncoding,
    compression: &CompressionConfig,
) -> Vec<u8> {
    use flate2::write::{DeflateEncoder, GzEncoder};
    use flate2::Compression;
    use std::io::Write;

    match encoding {
        AcceptedEncoding::Zstd => {
            zstd_compress_reused(body, compression.zstd_level).unwrap_or_else(|| body.to_vec())
        }
        AcceptedEncoding::Gzip => {
            let level = Compression::new(compression.gzip_level);
            let mut encoder = GzEncoder::new(Vec::with_capacity(body.len()), level);
            if encoder.write_all(body).is_err() {
                return body.to_vec();
            }
            encoder.finish().unwrap_or_else(|_| body.to_vec())
        }
        AcceptedEncoding::Brotli => {
            let mut compressed = Vec::with_capacity(body.len());
            let params = brotli::enc::BrotliEncoderParams {
                quality: compression.brotli_level as i32,
                ..Default::default()
            };
            let mut input = std::io::Cursor::new(body);
            if brotli::BrotliCompress(&mut input, &mut compressed, &params).is_err() {
                return body.to_vec();
            }
            compressed
        }
        AcceptedEncoding::Deflate => {
            let level = Compression::new(compression.gzip_level);
            let mut encoder = DeflateEncoder::new(Vec::with_capacity(body.len()), level);
            if encoder.write_all(body).is_err() {
                return body.to_vec();
            }
            encoder.finish().unwrap_or_else(|_| body.to_vec())
        }
        AcceptedEncoding::Identity => body.to_vec(),
    }
}

/// compression feature 無効時のスタブ
#[cfg(not(feature = "compression"))]
#[inline]
pub fn compress_body(
    body: &[u8],
    _encoding: AcceptedEncoding,
    _compression: &CompressionConfig,
) -> Vec<u8> {
    body.to_vec()
}

/// スレッドローカルの zstd 圧縮コンテキストを使い回して `body` を圧縮する（F-169 Part B・B-99）。
/// 失敗したら `None`。HTTP/1.1・HTTP/2・HTTP/3 で 1 スレッド 1 個を共有する。
#[cfg(feature = "compression")]
pub(crate) fn zstd_compress_reused(body: &[u8], level: i32) -> Option<Vec<u8>> {
    use std::cell::RefCell;

    thread_local! {
        static ZSTD_COMPRESSOR: RefCell<Option<zstd::bulk::Compressor<'static>>> =
            const { RefCell::new(None) };
    }

    ZSTD_COMPRESSOR.with(|cell| {
        let mut slot = cell.borrow_mut();
        let compressor = match slot.as_mut() {
            Some(c) => c,
            None => slot.get_or_insert(zstd::bulk::Compressor::new(level).ok()?),
        };
        compressor.set_compression_level(level).ok()?;
        compressor.compress(body).ok()
    })
}

const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// UNIX 秒を IMF-fixdate（`Sun, 06 Nov 1994 08:49:37 GMT`）にする
pub fn http_date(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        WEEKDAYS[(days.rem_euclid(7)) as usize],
        d,
        MONTHS[(m - 1) as usize],
        y,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// IMF-fixdate を UNIX 秒にする（RFC 850・asctime の形は受け付けない＝条件が無いのと同じ扱い）
pub fn parse_http_date(v: &[u8]) -> Option<u64> {
    let s = std::str::from_utf8(trim_ows(v)).ok()?;
    // "Sun, 06 Nov 1994 08:49:37 GMT"
    let rest = s.get(5..)?;
    let mut it = rest.split(' ');
    let day: u32 = it.next()?.parse().ok()?;
    let mon = it.next()?;
    let year: i64 = it.next()?.parse().ok()?;
    let time = it.next()?;
    if it.next()? != "GMT" || s.as_bytes().get(3) != Some(&b',') {
        return None;
    }
    let month = MONTHS.iter().position(|m| *m == mon)? as u32 + 1;
    let mut t = time.split(':');
    let hh: u64 = t.next()?.parse().ok()?;
    let mm: u64 = t.next()?.parse().ok()?;
    let ss: u64 = t.next()?.parse().ok()?;
    if day == 0 || day > 31 || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    if days < 0 {
        return None;
    }
    Some(days as u64 * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// 1970-01-01 からの日数 → (年, 月, 日)（H. Hinnant の civil_from_days）
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// (年, 月, 日) → 1970-01-01 からの日数（H. Hinnant の days_from_civil）
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = m as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comp(enabled: bool) -> CompressionConfig {
        CompressionConfig {
            enabled,
            ..Default::default()
        }
    }

    fn req<'a>(hs: &'a [(&'a [u8], &'a [u8])], method: &[u8]) -> StaticRequest<'a> {
        StaticRequest::from_headers(method, hs.iter().copied())
    }

    #[test]
    fn http_date_round_trip() {
        assert_eq!(http_date(784_111_777), "Sun, 06 Nov 1994 08:49:37 GMT");
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(
            parse_http_date(b"Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        for s in [0u64, 951_782_400, 1_700_000_000, 4_102_444_800] {
            assert_eq!(parse_http_date(http_date(s).as_bytes()), Some(s), "{s}");
        }
        assert_eq!(parse_http_date(b"Sunday, 06-Nov-94 08:49:37 GMT"), None);
        assert_eq!(parse_http_date(b"garbage"), None);
    }

    #[test]
    fn conditional_requests() {
        let v = Validators::for_file(
            10,
            Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000)),
            "text/plain",
        );
        assert_eq!(&v.etag[..], b"\"3e8-a\"");
        let c = comp(false);
        let etag = v.etag.clone();
        let weak = v.weak_etag.clone();
        let lm = v.last_modified.clone();
        let cases: [(&[(&[u8], &[u8])], Plan); 7] = [
            (&[(b"If-None-Match", &etag)], Plan::NotModified),
            (&[(b"if-none-match", &weak)], Plan::NotModified),
            (&[(b"if-none-match", b"\"x\", *")], Plan::NotModified),
            (
                &[(b"if-none-match", b"\"other\"")],
                Plan::Full { encoding: None },
            ),
            (&[(b"if-modified-since", &lm)], Plan::NotModified),
            (
                &[(b"if-modified-since", b"Thu, 01 Jan 1970 00:00:00 GMT")],
                Plan::Full { encoding: None },
            ),
            // If-None-Match があれば If-Modified-Since は見ない
            (
                &[
                    (b"if-none-match", b"\"other\""),
                    (b"if-modified-since", &lm),
                ],
                Plan::Full { encoding: None },
            ),
        ];
        for (hs, want) in cases {
            assert_eq!(plan(&req(hs, b"GET"), 10, "text/plain", Some(&v), &c), want);
        }
    }

    #[test]
    fn ranges_and_if_range() {
        let v = Validators::for_file(100, None, "text/plain");
        let c = comp(false);
        let p = |hs: &[(&[u8], &[u8])]| plan(&req(hs, b"GET"), 100, "text/plain", Some(&v), &c);
        assert_eq!(
            p(&[(b"range", b"bytes=0-9")]),
            Plan::Partial { start: 0, end: 9 }
        );
        assert_eq!(
            p(&[(b"range", b"bytes=-10")]),
            Plan::Partial { start: 90, end: 99 }
        );
        assert_eq!(
            p(&[(b"range", b"bytes=95-")]),
            Plan::Partial { start: 95, end: 99 }
        );
        assert_eq!(p(&[(b"range", b"bytes=200-300")]), Plan::Unsatisfiable);
        assert_eq!(
            p(&[(b"range", b"items=0-1")]),
            Plan::Full { encoding: None }
        );
        let etag = v.etag.clone();
        assert_eq!(
            p(&[(b"range", b"bytes=0-9"), (b"if-range", &etag)]),
            Plan::Partial { start: 0, end: 9 }
        );
        assert_eq!(
            p(&[(b"range", b"bytes=0-9"), (b"if-range", b"\"stale\"")]),
            Plan::Full { encoding: None }
        );
    }

    #[test]
    fn compression_wins_over_range() {
        let c = comp(true);
        let hs: &[(&[u8], &[u8])] = &[(b"range", b"bytes=0-9"), (b"accept-encoding", b"gzip")];
        let got = plan(&req(hs, b"GET"), 4096, "text/plain", None, &c);
        assert_eq!(
            got,
            Plan::Full {
                encoding: Some(AcceptedEncoding::Gzip)
            }
        );
        // 圧縮しない型は Range に応える
        let got = plan(&req(hs, b"GET"), 4096, "image/png", None, &c);
        assert_eq!(got, Plan::Partial { start: 0, end: 9 });
        assert!(compression_applies(&c, "text/plain", 4096));
        assert!(!compression_applies(&c, "text/plain", 10));
        assert!(!compression_applies(&comp(false), "text/plain", 4096));
        // B-283: その場で圧縮する上限を超えるファイルは圧縮せず、Range にも応える
        let big = MAX_STATIC_COMPRESS_SIZE + 1;
        let got = plan(&req(hs, b"GET"), big, "text/plain", None, &c);
        assert_eq!(got, Plan::Partial { start: 0, end: 9 });
        assert!(!compression_applies(&c, "text/plain", big));
        assert!(compression_applies(
            &c,
            "text/plain",
            MAX_STATIC_COMPRESS_SIZE
        ));
    }

    #[test]
    fn headers_per_plan() {
        let v = Validators::for_file(100, Some(SystemTime::UNIX_EPOCH), "text/plain");
        let collect = |p: Plan, vary: bool| {
            let mut out: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
            emit_headers(&p, 100, "text/plain", Some(&v), vary, |n, val| {
                out.push((n.to_vec(), val.as_slice().to_vec()))
            });
            out
        };
        let partial = collect(Plan::Partial { start: 0, end: 9 }, false);
        assert!(partial.contains(&(b"content-range".to_vec(), b"bytes 0-9/100".to_vec())));
        let unsat = collect(Plan::Unsatisfiable, true);
        assert!(unsat.contains(&(b"content-range".to_vec(), b"bytes */100".to_vec())));
        assert!(!unsat.iter().any(|(n, _)| n == b"etag" || n == b"vary"));
        let gz = collect(
            Plan::Full {
                encoding: Some(AcceptedEncoding::Gzip),
            },
            true,
        );
        assert!(gz.contains(&(b"etag".to_vec(), v.weak_etag.to_vec())));
        assert!(gz.contains(&(b"content-encoding".to_vec(), b"gzip".to_vec())));
        assert!(gz.contains(&(b"vary".to_vec(), b"Accept-Encoding".to_vec())));
        assert!(!gz.iter().any(|(n, _)| n == b"accept-ranges"));
        let nm = collect(Plan::NotModified, false);
        assert!(!nm.iter().any(|(n, _)| n == b"content-type"));
        assert!(nm.contains(&(b"etag".to_vec(), v.etag.to_vec())));
    }

    #[test]
    fn head_flag() {
        assert!(req(&[], b"HEAD").head);
        assert!(!req(&[], b"GET").head);
    }

    #[test]
    fn memory_validators_follow_content() {
        let a = Validators::for_memory(b"abc", None, "text/plain");
        let b = Validators::for_memory(b"abc", None, "text/plain");
        let c = Validators::for_memory(b"abd", None, "text/plain");
        assert_eq!(a.etag, b.etag);
        assert_ne!(a.etag, c.etag);
        assert!(a.last_modified.is_empty());
    }
}
