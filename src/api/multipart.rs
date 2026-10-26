// F-266: multipart/form-data（RFC 7578 + RFC 2046 §5.1）の解析器と抽出子。設計は
// docs/backlog/features/F-266-multipart-file-upload.md。
//
// 溜めた本文・ストリーミングの両方を同じ状態機械（`Core`）で扱う。本文のデータは受け取った `Bytes` の
// 切り出しで渡し（複製しない）、複製するのはパートのヘッダが断片の境目をまたいだとき・区切りの先頭に
// 見える末尾を持ち越したときだけ。境界の検索は `memchr::memmem`。
//! `multipart/form-data` bodies (forms with file uploads).
//!
//! - [`MultipartForm<T>`]: the non-file fields deserialized into `T` (the body is buffered, as for [`Form`]).
//! - [`UploadFile`]: one file field, named after the handler argument (`Vec<UploadFile>` for several,
//!   `Option<UploadFile>` when it may be absent). The data is a slice of the buffered body.
//! - [`Multipart`]: the body as it arrives (a `streaming` route), part by part and chunk by chunk, with
//!   [`Field::save_to_temp`] / [`Field::save_to`] to write a large file without holding it in memory.
//!
//! Limits come from `[api]`: `multipart_max_parts`, `multipart_max_field_size`, `multipart_max_file_size`,
//! `multipart_max_total_size` and `multipart_max_header_size` (413 when exceeded; a malformed body is 400).
//!
//! [`Form`]: super::extract::Form

use super::extract::{single_error, FromRequest};
use super::request::Request;
use super::response::Response;
use super::stream::RequestBodyStream;
use bytes::{Buf, Bytes, BytesMut};
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::RwLock;

// ===================== 設定（[api] multipart_*） =====================

static MAX_PARTS: AtomicUsize = AtomicUsize::new(100);
static MAX_FIELD: AtomicUsize = AtomicUsize::new(1024 * 1024);
// 0 = [api] max_body_size と同じ
static MAX_FILE: AtomicUsize = AtomicUsize::new(0);
// B-195: 本文全体の上限。0 = max_body_size（multipart_max_file_size の方が大きければそちら）
static MAX_TOTAL: AtomicUsize = AtomicUsize::new(0);
static MAX_HEADER: AtomicUsize = AtomicUsize::new(8 * 1024);
static SPOOL_THRESHOLD: AtomicUsize = AtomicUsize::new(1024 * 1024);
static TMP_DIR: RwLock<Option<PathBuf>> = RwLock::new(None);

pub(crate) fn apply_settings(s: &super::app::ApiSettings) {
    MAX_PARTS.store(s.multipart_max_parts.max(1), Ordering::Relaxed);
    MAX_FIELD.store(s.multipart_max_field_size.max(1), Ordering::Relaxed);
    MAX_FILE.store(s.multipart_max_file_size, Ordering::Relaxed);
    MAX_TOTAL.store(s.multipart_max_total_size, Ordering::Relaxed);
    MAX_HEADER.store(s.multipart_max_header_size.max(256), Ordering::Relaxed);
    SPOOL_THRESHOLD.store(s.multipart_spool_threshold, Ordering::Relaxed);
    *TMP_DIR.write().unwrap() = s.upload_tmp_dir.as_ref().map(PathBuf::from);
}

/// The directory for temporary upload files (`[api] upload_tmp_dir`, default the system temporary directory).
pub fn upload_tmp_dir() -> PathBuf {
    TMP_DIR
        .read()
        .unwrap()
        .clone()
        .unwrap_or_else(std::env::temp_dir)
}

/// Uploads up to this size are kept in memory by the Python `UploadFile` (`[api] multipart_spool_threshold`).
pub fn spool_threshold() -> usize {
    SPOOL_THRESHOLD.load(Ordering::Relaxed)
}

/// The limits applied to one body.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Maximum number of parts.
    pub max_parts: usize,
    /// Maximum size of a part without a file name.
    pub max_field_size: usize,
    /// Maximum size of a file part.
    pub max_file_size: usize,
    /// Maximum size of the whole body (preamble, part headers and data).
    pub max_total_size: usize,
    /// Maximum size of the headers of one part.
    pub max_header_size: usize,
}

impl Limits {
    /// The limits of `[api]`.
    pub fn current() -> Limits {
        let body = super::app::max_body_size();
        let file = match MAX_FILE.load(Ordering::Relaxed) {
            0 => body,
            n => n,
        };
        // 0 = max_body_size。ファイルの上限を明示的に上げた設定では 1 ファイル分は受けられるようにする
        let total = match MAX_TOTAL.load(Ordering::Relaxed) {
            0 => body.max(file),
            n => n,
        };
        Limits {
            max_parts: MAX_PARTS.load(Ordering::Relaxed),
            max_field_size: MAX_FIELD.load(Ordering::Relaxed),
            max_file_size: file,
            max_total_size: total,
            max_header_size: MAX_HEADER.load(Ordering::Relaxed),
        }
    }

    /// No limits other than the header size (tests and the fuzzer).
    pub fn unlimited() -> Limits {
        Limits {
            max_parts: usize::MAX,
            max_field_size: usize::MAX,
            max_file_size: usize::MAX,
            max_total_size: usize::MAX,
            max_header_size: 64 * 1024,
        }
    }
}

// ===================== 誤り =====================

/// A malformed body (400) or an exceeded limit (413).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultipartError {
    status: u16,
    message: Cow<'static, str>,
}

impl MultipartError {
    fn bad(m: &'static str) -> MultipartError {
        MultipartError {
            status: 400,
            message: Cow::Borrowed(m),
        }
    }
    fn too_large(m: &'static str) -> MultipartError {
        MultipartError {
            status: 413,
            message: Cow::Borrowed(m),
        }
    }
    /// 400 (malformed) or 413 (a limit).
    pub fn status(&self) -> u16 {
        self.status
    }
    /// The reason.
    pub fn message(&self) -> &str {
        &self.message
    }
    /// The response to send (`{"detail": "..."}` with the status).
    pub fn into_response(self) -> Response {
        Response::error(self.status, &self.message)
    }
}

impl std::fmt::Display for MultipartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.status)
    }
}

impl std::error::Error for MultipartError {}

impl From<MultipartError> for Response {
    fn from(e: MultipartError) -> Response {
        e.into_response()
    }
}

impl From<MultipartError> for super::response::ApiError {
    fn from(e: MultipartError) -> super::response::ApiError {
        super::response::ApiError::new(e.status, e.message.into_owned())
    }
}

// ===================== パートのヘッダ =====================

/// The headers of one part.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartInfo {
    /// `name` of `Content-Disposition`.
    pub name: Option<String>,
    /// `filename` (or `filename*`, RFC 5987, UTF-8) of `Content-Disposition`.
    pub filename: Option<String>,
    /// `Content-Type` of the part.
    pub content_type: Option<String>,
    /// All headers of the part (names in lower case).
    pub headers: Vec<(String, String)>,
}

impl PartInfo {
    /// Whether the part is a file (it has a file name, possibly empty).
    pub fn is_file(&self) -> bool {
        self.filename.is_some()
    }
}

/// The boundary of a `multipart/form-data` content type (`None` for another type or a missing / invalid boundary).
pub fn boundary(content_type: &str) -> Option<String> {
    let mut it = split_params(content_type);
    let ty = it.next()?;
    if !ty.trim().eq_ignore_ascii_case("multipart/form-data") {
        return None;
    }
    for p in it {
        let (k, v) = p.split_once('=')?;
        if k.trim().eq_ignore_ascii_case("boundary") {
            let v = unquote(v.trim());
            // RFC 2046: 1〜70 文字、最後が空白でない
            if v.is_empty() || v.len() > 70 || v.ends_with(' ') {
                return None;
            }
            return Some(v.into_owned());
        }
    }
    None
}

/// Splits `a; b="x;y"; c` at the semicolons outside quotes.
fn split_params(s: &str) -> impl Iterator<Item = &str> {
    let mut rest = Some(s);
    std::iter::from_fn(move || {
        let s = rest?;
        let mut quoted = false;
        let mut escaped = false;
        for (i, c) in s.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            match c {
                '\\' if quoted => escaped = true,
                '"' => quoted = !quoted,
                ';' if !quoted => {
                    rest = Some(&s[i + 1..]);
                    return Some(&s[..i]);
                }
                _ => {}
            }
        }
        rest = None;
        Some(s)
    })
}

fn unquote(v: &str) -> Cow<'_, str> {
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        let inner = &v[1..v.len() - 1];
        if !inner.contains('\\') {
            return Cow::Borrowed(inner);
        }
        let mut out = String::with_capacity(inner.len());
        let mut esc = false;
        for c in inner.chars() {
            if esc {
                out.push(c);
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else {
                out.push(c);
            }
        }
        Cow::Owned(out)
    } else {
        Cow::Borrowed(v)
    }
}

/// RFC 5987 `UTF-8''%E2%82%AC.txt` (only UTF-8 is accepted).
fn ext_value(v: &str) -> Option<String> {
    let (charset, rest) = v.split_once('\'')?;
    let (_lang, enc) = rest.split_once('\'')?;
    if !charset.eq_ignore_ascii_case("utf-8") {
        return None;
    }
    let mut out = Vec::with_capacity(enc.len());
    let b = enc.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let h = std::str::from_utf8(&b[i + 1..i + 3]).ok()?;
            out.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn parse_headers(block: &[u8]) -> Result<PartInfo, MultipartError> {
    let text = std::str::from_utf8(block)
        .map_err(|_| MultipartError::bad("part headers are not UTF-8"))?;
    let mut info = PartInfo::default();
    let mut disposition = false;
    for line in text.split("\r\n") {
        if line.is_empty() {
            continue;
        }
        if line.contains('\n') || line.contains('\r') {
            return Err(MultipartError::bad("multipart lines must end with CRLF"));
        }
        let (k, v) = line
            .split_once(':')
            .ok_or_else(|| MultipartError::bad("malformed part header"))?;
        let k = k.trim().to_ascii_lowercase();
        let v = v.trim();
        match k.as_str() {
            "content-disposition" => {
                let mut it = split_params(v);
                let ty = it.next().unwrap_or("").trim();
                if !ty.eq_ignore_ascii_case("form-data") {
                    return Err(MultipartError::bad("part is not form-data"));
                }
                disposition = true;
                let mut star: Option<String> = None;
                for p in it {
                    let Some((pk, pv)) = p.split_once('=') else {
                        continue;
                    };
                    let pk = pk.trim();
                    let pv = pv.trim();
                    if pk.eq_ignore_ascii_case("name") {
                        info.name = Some(unquote(pv).into_owned());
                    } else if pk.eq_ignore_ascii_case("filename") {
                        info.filename = Some(unquote(pv).into_owned());
                    } else if pk.eq_ignore_ascii_case("filename*") {
                        star = ext_value(unquote(pv).as_ref());
                    }
                }
                if star.is_some() {
                    info.filename = star;
                }
            }
            "content-type" => info.content_type = Some(v.to_string()),
            _ => {}
        }
        info.headers.push((k, v.to_string()));
    }
    if !disposition {
        return Err(MultipartError::bad("part has no Content-Disposition"));
    }
    Ok(info)
}

// ===================== 状態機械 =====================

/// What the machine needs or produced.
enum Step<T> {
    Ready(T),
    /// More input is needed (call `feed`).
    NeedMore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Before the first boundary (the preamble).
    Start,
    /// Just after a delimiter (`--` ends the body, CRLF starts a part).
    AfterDelim,
    /// The headers of a part.
    Headers,
    /// The data of a part.
    Data,
    /// The data of the part ended (the next call to `next_chunk` returns `None`).
    DataEnd,
    Done,
}

/// The parser shared by the buffered and streaming forms.
struct Core {
    /// Unprocessed input.
    cur: Bytes,
    /// No more input will come.
    eof: bool,
    /// `\r\n--boundary`.
    delim: Vec<u8>,
    finder: memchr::memmem::Finder<'static>,
    state: State,
    limits: Limits,
    parts: usize,
    part_len: usize,
    part_limit: usize,
    /// Preamble bytes discarded so far (bounded like headers).
    skipped: usize,
    /// Bytes fed so far (bounded by `max_total_size`).
    total: usize,
}

impl Core {
    fn new(boundary: &str, limits: Limits) -> Core {
        let mut delim = Vec::with_capacity(boundary.len() + 4);
        delim.extend_from_slice(b"\r\n--");
        delim.extend_from_slice(boundary.as_bytes());
        let finder = memchr::memmem::Finder::new(&delim).into_owned();
        Core {
            cur: Bytes::new(),
            eof: false,
            delim,
            finder,
            state: State::Start,
            limits,
            parts: 0,
            part_len: 0,
            part_limit: 0,
            skipped: 0,
            total: 0,
        }
    }

    /// Adds input (`None` = the end of the body).
    fn feed(&mut self, data: Option<Bytes>) {
        if let Some(d) = &data {
            self.total = self.total.saturating_add(d.len());
        }
        match data {
            None => self.eof = true,
            Some(d) if self.cur.is_empty() => self.cur = d,
            Some(d) => {
                // 持ち越し（ヘッダの途中・区切りの先頭に見える末尾）は小さいので、つないだ分だけ複製する
                let mut b = BytesMut::with_capacity(self.cur.len() + d.len());
                b.extend_from_slice(&self.cur);
                b.extend_from_slice(&d);
                self.cur = b.freeze();
            }
        }
    }

    fn need(&self, what: &'static str) -> Result<Step<()>, MultipartError> {
        if self.eof {
            Err(MultipartError::bad(what))
        } else {
            Ok(Step::NeedMore)
        }
    }

    /// The whole body exceeded `max_total_size` (checked before parsing what was fed).
    fn check_total(&self) -> Result<(), MultipartError> {
        if self.total > self.limits.max_total_size {
            return Err(MultipartError::too_large("multipart body is too large"));
        }
        Ok(())
    }

    /// Moves to the headers of the next part (skipping the rest of the current one). `None` at the end.
    fn next_part(&mut self) -> Result<Step<Option<PartInfo>>, MultipartError> {
        self.check_total()?;
        loop {
            match self.state {
                State::Done => return Ok(Step::Ready(None)),
                State::Data | State::DataEnd => match self.next_chunk()? {
                    Step::NeedMore => return Ok(Step::NeedMore),
                    Step::Ready(_) => {}
                },
                State::Start => {
                    let dash = &self.delim[2..];
                    let limit = self.limits.max_header_size.max(8192);
                    // 本文の先頭だけは CRLF 無しの "--boundary" でよい
                    if self.skipped == 0
                        && self.cur.len() >= dash.len()
                        && self.cur.starts_with(dash)
                    {
                        self.cur.advance(dash.len());
                        self.state = State::AfterDelim;
                        continue;
                    }
                    if let Some(i) = self.finder.find(&self.cur) {
                        if self.skipped + i > limit {
                            return Err(MultipartError::bad("multipart boundary not found"));
                        }
                        self.cur.advance(i + self.delim.len());
                        self.state = State::AfterDelim;
                        continue;
                    }
                    // 前置きは捨てる（区切りの先頭に見える末尾だけ残す）
                    let keep = partial_suffix(&self.cur, &self.delim).max(
                        if self.cur.len() < dash.len() {
                            self.cur.len()
                        } else {
                            0
                        },
                    );
                    let drop = self.cur.len() - keep;
                    self.skipped += drop;
                    self.cur.advance(drop);
                    if self.skipped > limit {
                        return Err(MultipartError::bad("multipart boundary not found"));
                    }
                    if let Step::NeedMore = self.need("multipart boundary not found")? {
                        return Ok(Step::NeedMore);
                    }
                }
                State::AfterDelim => {
                    if self.cur.len() < 2 {
                        if let Step::NeedMore = self.need("multipart body is truncated")? {
                            return Ok(Step::NeedMore);
                        }
                    }
                    if self.cur.starts_with(b"--") {
                        // 終端。後置きは見ない
                        self.state = State::Done;
                        self.cur = Bytes::new();
                        return Ok(Step::Ready(None));
                    }
                    // 区切りの後の空白（transport padding）
                    let pad = self
                        .cur
                        .iter()
                        .take_while(|&&c| c == b' ' || c == b'\t')
                        .count();
                    if pad > 64 {
                        return Err(MultipartError::bad("malformed multipart boundary"));
                    }
                    if pad == self.cur.len() || self.cur.len() < pad + 2 {
                        if let Step::NeedMore = self.need("multipart body is truncated")? {
                            return Ok(Step::NeedMore);
                        }
                        continue;
                    }
                    if &self.cur[pad..pad + 2] != b"\r\n" {
                        return Err(if self.cur[pad] == b'\n' {
                            MultipartError::bad("multipart lines must end with CRLF")
                        } else {
                            MultipartError::bad("malformed multipart boundary")
                        });
                    }
                    self.cur.advance(pad + 2);
                    self.state = State::Headers;
                }
                State::Headers => {
                    let (block_end, data_start) = if self.cur.starts_with(b"\r\n") {
                        (0, 2)
                    } else if let Some(i) = memchr::memmem::find(&self.cur, b"\r\n\r\n") {
                        (i, i + 4)
                    } else {
                        // 終端の "\r\n\r\n" が上限ちょうどで終わる場合があるので 3 バイトの余裕を見る
                        if self.cur.len() > self.limits.max_header_size + 3 {
                            return Err(MultipartError::too_large(
                                "multipart part headers are too large",
                            ));
                        }
                        if let Step::NeedMore = self.need("multipart body is truncated")? {
                            return Ok(Step::NeedMore);
                        }
                        continue;
                    };
                    if block_end > self.limits.max_header_size {
                        return Err(MultipartError::too_large(
                            "multipart part headers are too large",
                        ));
                    }
                    let info = parse_headers(&self.cur[..block_end])?;
                    self.cur.advance(data_start);
                    self.parts += 1;
                    if self.parts > self.limits.max_parts {
                        return Err(MultipartError::too_large("too many multipart parts"));
                    }
                    self.part_len = 0;
                    self.part_limit = if info.is_file() {
                        self.limits.max_file_size
                    } else {
                        self.limits.max_field_size
                    };
                    self.state = State::Data;
                    return Ok(Step::Ready(Some(info)));
                }
            }
        }
    }

    /// The next piece of the current part's data (`None` when the part ended).
    fn next_chunk(&mut self) -> Result<Step<Option<Bytes>>, MultipartError> {
        self.check_total()?;
        loop {
            match self.state {
                State::Data => {}
                State::DataEnd => {
                    self.state = State::AfterDelim;
                    return Ok(Step::Ready(None));
                }
                _ => return Ok(Step::Ready(None)),
            }
            let (chunk, end) = if let Some(i) = self.finder.find(&self.cur) {
                let c = self.cur.split_to(i);
                self.cur.advance(self.delim.len());
                (c, true)
            } else {
                let keep = partial_suffix(&self.cur, &self.delim);
                if self.cur.len() == keep {
                    if let Step::NeedMore =
                        self.need("multipart body is truncated (no closing boundary)")?
                    {
                        return Ok(Step::NeedMore);
                    }
                    continue;
                }
                (self.cur.split_to(self.cur.len() - keep), false)
            };
            self.part_len += chunk.len();
            if self.part_len > self.part_limit {
                return Err(MultipartError::too_large(
                    if self.part_limit == self.limits.max_file_size {
                        "multipart file is too large"
                    } else {
                        "multipart field is too large"
                    },
                ));
            }
            if end {
                if chunk.is_empty() {
                    self.state = State::AfterDelim;
                    return Ok(Step::Ready(None));
                }
                self.state = State::DataEnd;
            }
            return Ok(Step::Ready(Some(chunk)));
        }
    }
}

/// The length of the longest suffix of `data` that is a proper prefix of `delim` (it may continue in the next chunk).
fn partial_suffix(data: &[u8], delim: &[u8]) -> usize {
    let max = (delim.len() - 1).min(data.len());
    // 区切りは '\r' で始まるので、末尾の max バイトの中の '\r' からだけ試す
    let tail = &data[data.len() - max..];
    let mut from = 0;
    while let Some(off) = memchr::memchr(b'\r', &tail[from..]) {
        let s = &tail[from + off..];
        if delim.starts_with(s) {
            return s.len();
        }
        from += off + 1;
    }
    0
}

// ===================== 溜めた本文 =====================

/// One part of a buffered body.
#[derive(Debug, Clone)]
pub struct Part {
    /// The headers.
    pub info: PartInfo,
    /// The data (a slice of the body).
    pub data: Bytes,
}

/// Iterates over the parts of a buffered body.
pub struct Parts {
    core: Core,
    failed: bool,
}

impl Iterator for Parts {
    type Item = Result<Part, MultipartError>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        let r = (|| {
            let info = match self.core.next_part()? {
                Step::Ready(Some(i)) => i,
                Step::Ready(None) => return Ok(None),
                Step::NeedMore => return Err(MultipartError::bad("multipart body is truncated")),
            };
            // 全部あるので 1 回で区切りまで見つかる（データは本文の切り出し）
            let data = match self.core.next_chunk()? {
                Step::Ready(Some(d)) => {
                    // 次の呼び出しで DataEnd を抜ける
                    let _ = self.core.next_chunk()?;
                    d
                }
                Step::Ready(None) => Bytes::new(),
                Step::NeedMore => return Err(MultipartError::bad("multipart body is truncated")),
            };
            Ok(Some(Part { info, data }))
        })();
        match r {
            Ok(Some(p)) => Some(Ok(p)),
            Ok(None) => None,
            Err(e) => {
                self.failed = true;
                Some(Err(e))
            }
        }
    }
}

/// Parses a buffered body.
pub fn parse(body: Bytes, boundary: &str, limits: Limits) -> Parts {
    let mut core = Core::new(boundary, limits);
    core.feed(Some(body));
    core.feed(None);
    Parts {
        core,
        failed: false,
    }
}

/// The boundary of the request (`Content-Type: multipart/form-data; boundary=...`), or 400.
fn request_boundary(req: &Request) -> Result<String, MultipartError> {
    req.header("content-type")
        .and_then(boundary)
        .ok_or_else(|| MultipartError::bad("expected a multipart/form-data body with a boundary"))
}

/// Whether the request body is `multipart/form-data`.
pub fn is_multipart(req: &Request) -> bool {
    req.header("content-type").is_some_and(|ct| {
        ct.split(';')
            .next()
            .is_some_and(|t| t.trim().eq_ignore_ascii_case("multipart/form-data"))
    })
}

/// All parts of the request's buffered body.
pub fn request_parts(req: &Request) -> Result<Vec<Part>, MultipartError> {
    let b = request_boundary(req)?;
    parse(req.body.clone(), &b, Limits::current()).collect()
}

/// The non-file fields of a multipart body as `(name, value)` pairs (for `Form<T>` and `MultipartForm<T>`).
pub(crate) fn text_fields(req: &Request) -> Result<Vec<(String, String)>, Response> {
    let parts = request_parts(req).map_err(MultipartError::into_response)?;
    let mut out = Vec::with_capacity(parts.len());
    for p in parts {
        if p.info.is_file() {
            continue;
        }
        let Some(name) = p.info.name else { continue };
        let v = String::from_utf8(p.data.to_vec()).map_err(|_| {
            single_error(
                "body",
                Some(&name),
                "string_unicode",
                "Input should be a valid UTF-8 string",
            )
        })?;
        out.push((name, v));
    }
    Ok(out)
}

/// Deserializes `(name, value)` pairs with the rules of `application/x-www-form-urlencoded`.
pub(crate) fn fields_into<T: serde::de::DeserializeOwned>(
    fields: &[(String, String)],
) -> Result<T, Response> {
    let enc = serde_urlencoded::to_string(fields)
        .map_err(|e| single_error("body", None, "value_error", &e.to_string()))?;
    serde_urlencoded::from_str::<T>(&enc)
        .map_err(|e| single_error("body", None, "value_error", &e.to_string()))
}

// ===================== 抽出子（溜めた本文） =====================

/// The non-file fields of a `multipart/form-data` body deserialized into `T` (file parts are ignored).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MultipartForm<T>(pub T);

impl<T> std::ops::Deref for MultipartForm<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

/// Deserializes the fields of a multipart body into `T` (used by `MultipartForm<T>`).
pub fn multipart_form<T: serde::de::DeserializeOwned>(req: &Request) -> Result<T, Response> {
    let fields = text_fields(req)?;
    fields_into(&fields)
}

/// One uploaded file of a buffered `multipart/form-data` body.
#[derive(Debug, Clone)]
pub struct UploadFile {
    name: String,
    filename: String,
    content_type: Option<String>,
    headers: Vec<(String, String)>,
    data: Bytes,
}

impl UploadFile {
    fn from_part(p: Part) -> UploadFile {
        UploadFile {
            name: p.info.name.unwrap_or_default(),
            filename: p.info.filename.unwrap_or_default(),
            content_type: p.info.content_type,
            headers: p.info.headers,
            data: p.data,
        }
    }
    /// The field name.
    pub fn name(&self) -> &str {
        &self.name
    }
    /// The file name sent by the client (do not use it as a path as is).
    pub fn file_name(&self) -> &str {
        &self.filename
    }
    /// `Content-Type` of the part.
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }
    /// All headers of the part.
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }
    /// The contents (a slice of the request body; cloning is cheap).
    pub fn bytes(&self) -> &Bytes {
        &self.data
    }
    /// The contents, consuming the value.
    pub fn into_bytes(self) -> Bytes {
        self.data
    }
    /// The size in bytes.
    pub fn len(&self) -> usize {
        self.data.len()
    }
    /// Whether the file is empty.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
    /// The contents as UTF-8 text.
    pub fn text(&self) -> Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.data)
    }
}

fn file_parts(req: &Request, name: &str) -> Result<Vec<UploadFile>, Response> {
    let parts = request_parts(req).map_err(MultipartError::into_response)?;
    Ok(parts
        .into_iter()
        .filter(|p| p.info.is_file() && p.info.name.as_deref() == Some(name))
        .map(UploadFile::from_part)
        .collect())
}

/// The file field `name` (absent gives 422 with `loc = ["body", name]`, as in FastAPI).
pub fn upload_file(req: &Request, name: &str) -> Result<UploadFile, Response> {
    file_parts(req, name)?
        .into_iter()
        .next()
        .ok_or_else(|| single_error("body", Some(name), "missing", "Field required"))
}

/// The file field `name`, or `None`.
pub fn upload_file_opt(req: &Request, name: &str) -> Result<Option<UploadFile>, Response> {
    Ok(file_parts(req, name)?.into_iter().next())
}

/// All files of the field `name` (at least one, as `list[UploadFile]` in FastAPI).
pub fn upload_files(req: &Request, name: &str) -> Result<Vec<UploadFile>, Response> {
    let v = file_parts(req, name)?;
    if v.is_empty() {
        return Err(single_error(
            "body",
            Some(name),
            "missing",
            "Field required",
        ));
    }
    Ok(v)
}

/// The OpenAPI request body of a multipart route: the fields of `form` (resolved from `defs`) and the file
/// fields `(name, multiple, required)`, as one inline object schema (Swagger UI shows a file picker for each file).
pub fn multipart_body_meta(
    form: Option<serde_json::Value>,
    defs: &super::schema::Definitions,
    files: &[(&str, bool, bool)],
) -> super::handler::BodyMeta {
    use serde_json::{json, Map, Value};
    let resolved = match form {
        Some(f) => match f.get("$ref").and_then(|r| r.as_str()) {
            Some(r) => defs
                .get(r.trim_start_matches("#/components/schemas/"))
                .cloned()
                .unwrap_or(f),
            None => f,
        },
        None => json!({}),
    };
    let mut props: Map<String, Value> = resolved
        .get("properties")
        .and_then(|p| p.as_object())
        .cloned()
        .unwrap_or_default();
    let mut required: Vec<Value> = resolved
        .get("required")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    for (name, many, req) in files {
        let file = json!({"type": "string", "format": "binary"});
        props.insert(
            (*name).to_string(),
            if *many {
                json!({"type": "array", "items": file})
            } else {
                file
            },
        );
        if *req {
            required.push(Value::from(*name));
        }
    }
    let mut schema = json!({"type": "object", "properties": props});
    if !required.is_empty() {
        schema["required"] = Value::Array(required);
    }
    super::handler::BodyMeta {
        content_type: "multipart/form-data",
        schema,
        required: files.iter().any(|f| f.2) || resolved.get("required").is_some(),
    }
}

// ===================== ストリーミング =====================

/// A `multipart/form-data` body read as it arrives (on a `streaming` route; on other routes the buffered body is
/// read the same way).
pub struct Multipart {
    core: Core,
    stream: RequestBodyStream,
}

impl std::fmt::Debug for Multipart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Multipart")
    }
}

impl FromRequest for Multipart {
    fn from_request(req: &Request) -> Result<Self, Response> {
        let b = request_boundary(req).map_err(MultipartError::into_response)?;
        Ok(Multipart::new(
            req.take_body_stream(),
            &b,
            Limits::current(),
        ))
    }
}

impl Multipart {
    /// Reads `stream` with the boundary `boundary`.
    pub fn new(stream: RequestBodyStream, boundary: &str, limits: Limits) -> Multipart {
        Multipart {
            core: Core::new(boundary, limits),
            stream,
        }
    }

    async fn fill(&mut self) {
        use futures::StreamExt;
        let next = self.stream.next().await;
        self.core.feed(next);
    }

    /// The next part (the rest of the previous part is skipped). `None` after the last part.
    pub async fn next_part(&mut self) -> Result<Option<Field<'_>>, MultipartError> {
        loop {
            match self.core.next_part()? {
                Step::Ready(Some(info)) => return Ok(Some(Field { mp: self, info })),
                Step::Ready(None) => return Ok(None),
                Step::NeedMore => self.fill().await,
            }
        }
    }

    async fn chunk(&mut self) -> Result<Option<Bytes>, MultipartError> {
        loop {
            match self.core.next_chunk()? {
                Step::Ready(c) => return Ok(c),
                Step::NeedMore => self.fill().await,
            }
        }
    }
}

/// One part of a [`Multipart`] body.
pub struct Field<'a> {
    mp: &'a mut Multipart,
    info: PartInfo,
}

impl Field<'_> {
    /// The field name.
    pub fn name(&self) -> Option<&str> {
        self.info.name.as_deref()
    }
    /// The file name sent by the client (`None` for a plain field; do not use it as a path as is).
    pub fn file_name(&self) -> Option<&str> {
        self.info.filename.as_deref()
    }
    /// `Content-Type` of the part.
    pub fn content_type(&self) -> Option<&str> {
        self.info.content_type.as_deref()
    }
    /// All headers of the part.
    pub fn headers(&self) -> &[(String, String)] {
        &self.info.headers
    }
    /// The headers as one value.
    pub fn info(&self) -> &PartInfo {
        &self.info
    }

    /// The next piece of the data as it arrived (`None` at the end of the part).
    pub async fn chunk(&mut self) -> Result<Option<Bytes>, MultipartError> {
        self.mp.chunk().await
    }

    /// The whole data (kept in memory; bounded by the field / file size limit).
    pub async fn bytes(&mut self) -> Result<Bytes, MultipartError> {
        let first = match self.chunk().await? {
            Some(c) => c,
            None => return Ok(Bytes::new()),
        };
        let second = match self.chunk().await? {
            Some(c) => c,
            None => return Ok(first),
        };
        let mut b = BytesMut::with_capacity(first.len() + second.len());
        b.extend_from_slice(&first);
        b.extend_from_slice(&second);
        while let Some(c) = self.chunk().await? {
            b.extend_from_slice(&c);
        }
        Ok(b.freeze())
    }

    /// The whole data as UTF-8 text (400 if it is not UTF-8).
    pub async fn text(&mut self) -> Result<String, MultipartError> {
        let b = self.bytes().await?;
        String::from_utf8(b.to_vec())
            .map_err(|_| MultipartError::bad("multipart field is not UTF-8"))
    }

    /// Writes the data to an anonymous temporary file in `[api] upload_tmp_dir` (removed when the returned value
    /// is dropped). The writes run on the blocking pool.
    pub async fn save_to_temp(&mut self) -> Result<TempUpload, MultipartError> {
        let dir = upload_tmp_dir();
        let file = crate::runtime::offload::offload(move || temp_file_in(&dir))
            .await
            .map_err(|_| MultipartError {
                status: 500,
                message: Cow::Borrowed("cannot create a temporary upload file"),
            })?;
        let (file, len) = self.write_into(file).await?;
        Ok(TempUpload {
            file,
            len,
            filename: self.info.filename.clone(),
            content_type: self.info.content_type.clone(),
        })
    }

    /// Writes the data to `path` (created or truncated). The writes run on the blocking pool. Returns the size.
    pub async fn save_to(&mut self, path: impl AsRef<Path>) -> Result<u64, MultipartError> {
        let path = path.as_ref().to_path_buf();
        let file = crate::runtime::offload::offload(move || std::fs::File::create(path))
            .await
            .map_err(|_| MultipartError {
                status: 500,
                message: Cow::Borrowed("cannot create the upload file"),
            })?;
        let (_file, len) = self.write_into(file).await?;
        Ok(len)
    }

    /// Keeps the data in memory up to `threshold` bytes and moves it to an anonymous temporary file beyond that
    /// (what the Python `UploadFile` uses).
    pub async fn spool(&mut self, threshold: usize) -> Result<SpooledData, MultipartError> {
        let mut mem: Option<BytesMut> = None;
        let mut first: Option<Bytes> = None;
        loop {
            let Some(c) = self.chunk().await? else {
                return Ok(SpooledData::Memory(match (first, mem) {
                    (_, Some(m)) => m.freeze(),
                    (Some(f), None) => f,
                    (None, None) => Bytes::new(),
                }));
            };
            let held = mem.as_ref().map_or(0, |m| m.len()) + first.as_ref().map_or(0, |f| f.len());
            if held + c.len() <= threshold {
                // 1 つ目の断片は切り出しのまま持つ（小さなファイルは複製しない）
                match (&mut mem, &first) {
                    (None, None) => first = Some(c),
                    (None, Some(f)) => {
                        let mut m = BytesMut::with_capacity(f.len() + c.len());
                        m.extend_from_slice(f);
                        m.extend_from_slice(&c);
                        first = None;
                        mem = Some(m);
                    }
                    (Some(m), _) => m.extend_from_slice(&c),
                }
                continue;
            }
            // しきい値を超えた: ここまでの分と残りを一時ファイルへ
            let dir = upload_tmp_dir();
            let mut file = crate::runtime::offload::offload(move || temp_file_in(&dir))
                .await
                .map_err(|_| MultipartError {
                    status: 500,
                    message: Cow::Borrowed("cannot create a temporary upload file"),
                })?;
            let mut len = 0u64;
            for b in [first.take(), mem.take().map(BytesMut::freeze), Some(c)]
                .into_iter()
                .flatten()
            {
                len += b.len() as u64;
                file = write_chunk(file, b).await?;
            }
            while let Some(c) = self.chunk().await? {
                len += c.len() as u64;
                file = write_chunk(file, c).await?;
            }
            return Ok(SpooledData::File { file, len });
        }
    }

    async fn write_into(
        &mut self,
        mut file: std::fs::File,
    ) -> Result<(std::fs::File, u64), MultipartError> {
        let mut len = 0u64;
        while let Some(c) = self.chunk().await? {
            len += c.len() as u64;
            file = write_chunk(file, c).await?;
        }
        Ok((file, len))
    }
}

/// Writes `data` to `file` on the blocking pool.
async fn write_chunk(
    mut file: std::fs::File,
    data: Bytes,
) -> Result<std::fs::File, MultipartError> {
    let (f, r) = crate::runtime::offload::offload(move || {
        let r = std::io::Write::write_all(&mut file, &data);
        (file, r)
    })
    .await;
    r.map_err(|_| MultipartError {
        status: 500,
        message: Cow::Borrowed("cannot write the upload file"),
    })?;
    Ok(f)
}

/// The data of a spooled part ([`Field::spool`]).
#[derive(Debug)]
pub enum SpooledData {
    /// Kept in memory.
    Memory(Bytes),
    /// In an anonymous temporary file (positioned at its end).
    File {
        /// The file.
        file: std::fs::File,
        /// Its size in bytes.
        len: u64,
    },
}

/// One part read by [`prepare_form`].
#[derive(Debug)]
pub struct SpooledPart {
    /// The headers.
    pub info: PartInfo,
    /// The data.
    pub data: SpooledData,
}

/// Reads a form body before a handler that is not written against a stream runs (used by the Python and C
/// bindings): a `multipart/form-data` body is read part by part, keeping parts up to `[api]
/// multipart_spool_threshold` in memory and writing larger files to anonymous temporary files; another body is
/// collected into `Request::body` (up to `[api] max_body_size`).
pub async fn prepare_form(req: Request) -> Result<(Request, Option<Vec<SpooledPart>>), Response> {
    use futures::StreamExt;
    if is_multipart(&req) {
        let mut mp = Multipart::from_request(&req)?;
        let threshold = spool_threshold();
        let mut out = Vec::new();
        while let Some(mut f) = mp
            .next_part()
            .await
            .map_err(MultipartError::into_response)?
        {
            let info = f.info().clone();
            let data = if info.is_file() {
                f.spool(threshold).await
            } else {
                f.bytes().await.map(SpooledData::Memory)
            }
            .map_err(MultipartError::into_response)?;
            out.push(SpooledPart { info, data });
        }
        return Ok((req, Some(out)));
    }
    let mut req = req;
    if req.is_body_streaming() {
        let limit = super::app::max_body_size();
        let mut s = req.take_body_stream();
        let mut b = BytesMut::new();
        while let Some(c) = s.next().await {
            if b.len() + c.len() > limit {
                return Err(Response::error(413, "Payload Too Large"));
            }
            b.extend_from_slice(&c);
        }
        // B-187: 途中で切れた本文を全体として渡さない
        if s.is_aborted() {
            return Err(Response::error(400, "Bad Request"));
        }
        req.body = b.freeze();
        req.body_stream = None;
    }
    Ok((req, None))
}

/// An uploaded file written to an anonymous temporary file ([`Field::save_to_temp`]).
#[derive(Debug)]
pub struct TempUpload {
    file: std::fs::File,
    len: u64,
    filename: Option<String>,
    content_type: Option<String>,
}

impl TempUpload {
    /// The size in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }
    /// Whether the file is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    /// The file name sent by the client.
    pub fn file_name(&self) -> Option<&str> {
        self.filename.as_deref()
    }
    /// `Content-Type` of the part.
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }
    /// The open file (positioned at its end; it has no name and disappears when closed). Read it with blocking
    /// I/O only off the event loop (for example with `veil::api::__private::offload` or a synchronous handler).
    pub fn file(&self) -> &std::fs::File {
        &self.file
    }
    /// The open file.
    pub fn into_file(self) -> std::fs::File {
        self.file
    }
}

/// Creates an anonymous file in `dir` (Linux: `O_TMPFILE`; elsewhere a created and immediately removed file).
/// Blocking: call it inside `offload`.
pub(crate) fn temp_file_in(dir: &Path) -> std::io::Result<std::fs::File> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_TMPFILE)
            .mode(0o600)
            .open(dir)
        {
            Ok(f) => return Ok(f),
            // O_TMPFILE の無いファイルシステムは下の方法へ
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(libc::EOPNOTSUPP) | Some(libc::EISDIR)
                ) => {}
            Err(e) => return Err(e),
        }
    }
    for _ in 0..16 {
        let mut name = [0u8; 12];
        fill_random(&mut name);
        let p = dir.join(format!(
            "veil-upload-{}",
            name.iter()
                .map(|b| format!("{:02x}", b))
                .collect::<String>()
        ));
        let mut o = std::fs::OpenOptions::new();
        o.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        match o.open(&p) {
            Ok(f) => {
                // Windows は開いたファイルを消せないので、閉じたときに消す指定が要る（下の分岐）
                // offload の閉包の中だけで呼ぶ（呼び出し元は Field::save_to_temp・spool）
                #[cfg(unix)]
                #[allow(clippy::disallowed_methods)]
                let _ = std::fs::remove_file(&p);
                #[cfg(not(unix))]
                {
                    drop(f);
                    return windows_delete_on_close(&p);
                }
                #[cfg(unix)]
                return Ok(f);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "cannot create a unique temporary file",
    ))
}

#[cfg(not(unix))]
fn windows_delete_on_close(p: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    // FILE_FLAG_DELETE_ON_CLOSE は DELETE のアクセス権で開いたハンドルにしか付けられない（read・write だけだと
    // ERROR_INVALID_PARAMETER で開けない）。GENERIC_READ | GENERIC_WRITE | DELETE
    std::fs::OpenOptions::new()
        .access_mode(0x8000_0000 | 0x4000_0000 | 0x0001_0000)
        .custom_flags(0x0400_0000)
        .share_mode(0x7)
        .open(p)
}

fn fill_random(buf: &mut [u8]) {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0),
    );
    for chunk in buf.chunks_mut(8) {
        let v = h.finish().to_le_bytes();
        chunk.copy_from_slice(&v[..chunk.len()]);
        h.write_u64(u64::from_le_bytes(v));
    }
}

/// Fuzzing entry point: parses `data` buffered and streamed in two pieces and checks both give the same parts.
#[doc(hidden)]
pub fn fuzz_parse(data: &[u8]) {
    if data.len() < 2 {
        return;
    }
    let split = data[0] as usize;
    let body = Bytes::copy_from_slice(&data[1..]);
    let buffered: Vec<_> = parse(body.clone(), "XyZ", Limits::unlimited())
        .map(|r| r.map(|p| (p.info, p.data)))
        .collect();
    let mut core = Core::new("XyZ", Limits::unlimited());
    let cut = split.min(body.len());
    let mut pieces = vec![body.slice(..cut), body.slice(cut..)].into_iter();
    let mut streamed = Vec::new();
    'outer: loop {
        let info = loop {
            match core.next_part() {
                Ok(Step::Ready(Some(i))) => break i,
                Ok(Step::Ready(None)) => break 'outer,
                Ok(Step::NeedMore) => core.feed(pieces.next()),
                Err(e) => {
                    streamed.push(Err(e));
                    break 'outer;
                }
            }
        };
        let mut d = Vec::new();
        loop {
            match core.next_chunk() {
                Ok(Step::Ready(Some(c))) => d.extend_from_slice(&c),
                Ok(Step::Ready(None)) => break,
                Ok(Step::NeedMore) => core.feed(pieces.next()),
                Err(e) => {
                    streamed.push(Err(e));
                    break 'outer;
                }
            }
        }
        streamed.push(Ok((info, Bytes::from(d))));
    }
    assert_eq!(buffered, streamed);
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: &str = "----veilB0undary";

    fn body(parts: &[(&str, Option<&str>, &[u8])]) -> Vec<u8> {
        let mut v = Vec::new();
        for (name, file, data) in parts {
            v.extend_from_slice(format!("--{}\r\n", B).as_bytes());
            match file {
                Some(f) => v.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\nContent-Type: application/octet-stream\r\n\r\n",
                        name, f
                    )
                    .as_bytes(),
                ),
                None => v.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{}\"\r\n\r\n", name).as_bytes(),
                ),
            }
            v.extend_from_slice(data);
            v.extend_from_slice(b"\r\n");
        }
        v.extend_from_slice(format!("--{}--\r\n", B).as_bytes());
        v
    }

    fn all(b: &[u8], limits: Limits) -> Result<Vec<Part>, MultipartError> {
        parse(Bytes::copy_from_slice(b), B, limits).collect()
    }

    /// Streams `b` in pieces of `n` bytes and returns the parts.
    fn streamed(b: &[u8], n: usize) -> Result<Vec<(PartInfo, Vec<u8>)>, MultipartError> {
        streamed_with(b, n, Limits::unlimited())
    }

    fn streamed_with(
        b: &[u8],
        n: usize,
        limits: Limits,
    ) -> Result<Vec<(PartInfo, Vec<u8>)>, MultipartError> {
        let (mut tx, slot) = crate::api::stream::request_body_channel(b.len() / n + 2);
        for c in b.chunks(n) {
            tx.try_send(Bytes::copy_from_slice(c)).unwrap();
        }
        drop(tx);
        let s = slot.lock().unwrap().take().unwrap();
        let mut mp = Multipart::new(s, B, limits);
        futures::executor::block_on(async {
            let mut out = Vec::new();
            while let Some(mut f) = mp.next_part().await? {
                let info = f.info().clone();
                let d = f.bytes().await?;
                out.push((info, d.to_vec()));
            }
            Ok(out)
        })
    }

    #[test]
    fn boundary_parsing() {
        assert_eq!(
            boundary("multipart/form-data; boundary=abc").as_deref(),
            Some("abc")
        );
        assert_eq!(
            boundary("Multipart/Form-Data;boundary=\"a b;c\"").as_deref(),
            Some("a b;c")
        );
        assert_eq!(
            boundary("multipart/form-data; charset=x; boundary=z").as_deref(),
            Some("z")
        );
        assert_eq!(boundary("multipart/form-data"), None);
        assert_eq!(boundary("application/json; boundary=x"), None);
        assert_eq!(
            boundary(&format!("multipart/form-data; boundary={}", "x".repeat(71))),
            None
        );
    }

    #[test]
    fn rfc7578_example_and_fields() {
        // RFC 7578 §4.6 の形（前置き・後置き付き）
        let b = format!(
            "preamble\r\n--{B}\r\nContent-Disposition: form-data; name=\"field1\"\r\nContent-Type: text/plain;charset=UTF-8\r\n\r\nJoe owes =E2=82=AC100.\r\n--{B}\r\nContent-Disposition: form-data; name=\"f\"; filename*=UTF-8''%E2%82%AC%20rates.txt\r\n\r\n\r\n--{B}--\r\nepilogue"
        );
        let parts = all(b.as_bytes(), Limits::unlimited()).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].info.name.as_deref(), Some("field1"));
        assert_eq!(
            parts[0].info.content_type.as_deref(),
            Some("text/plain;charset=UTF-8")
        );
        assert_eq!(&parts[0].data[..], b"Joe owes =E2=82=AC100.");
        assert_eq!(
            parts[1].info.filename.as_deref(),
            Some("\u{20ac} rates.txt")
        );
        assert!(parts[1].data.is_empty());
    }

    #[test]
    fn data_is_a_slice_of_the_body() {
        let raw = Bytes::from(body(&[("a", Some("x.bin"), &[0u8; 1000])]));
        let parts: Vec<Part> = parse(raw.clone(), B, Limits::unlimited())
            .collect::<Result<_, _>>()
            .unwrap();
        let start = parts[0].data.as_ptr() as usize - raw.as_ptr() as usize;
        assert!(start > 0 && start + 1000 <= raw.len());
    }

    #[test]
    fn every_split_position_streams_the_same_parts() {
        let data: Vec<u8> = (0..300u32).map(|i| (i % 7) as u8 + b'\r' - 3).collect();
        let raw = body(&[
            ("t", None, b"hello"),
            ("f", Some("a.bin"), &data),
            ("e", None, b""),
            ("g", Some("b"), b"\r\n--"),
        ]);
        let expect: Vec<(PartInfo, Vec<u8>)> = all(&raw, Limits::unlimited())
            .unwrap()
            .into_iter()
            .map(|p| (p.info, p.data.to_vec()))
            .collect();
        assert_eq!(expect.len(), 4);
        assert_eq!(expect[3].1, b"\r\n--");
        for n in 1..raw.len() {
            assert_eq!(streamed(&raw, n).unwrap(), expect, "chunk size {}", n);
        }
    }

    #[test]
    fn limits_give_413() {
        let raw = body(&[("a", None, b"0123456789")]);
        let mut l = Limits::unlimited();
        l.max_field_size = 5;
        assert_eq!(all(&raw, l).unwrap_err().status(), 413);
        let raw = body(&[("a", Some("f"), b"0123456789")]);
        l = Limits::unlimited();
        l.max_file_size = 9;
        assert_eq!(all(&raw, l).unwrap_err().status(), 413);
        l.max_file_size = 10;
        assert!(all(&raw, l).is_ok());
        let raw = body(&[("a", None, b"1"), ("b", None, b"2"), ("c", None, b"3")]);
        l = Limits::unlimited();
        l.max_parts = 2;
        assert_eq!(all(&raw, l).unwrap_err().status(), 413);
        l = Limits::unlimited();
        l.max_header_size = 300;
        let big = format!(
            "--{B}\r\nContent-Disposition: form-data; name=\"{}\"\r\n\r\nx\r\n--{B}--",
            "n".repeat(400)
        );
        assert_eq!(all(big.as_bytes(), l).unwrap_err().status(), 413);
    }

    // B-195: パートごとの上限の内側でも、本文全体が max_total_size を超えたら 413
    #[test]
    fn total_size_limit_gives_413() {
        let parts: Vec<(String, Vec<u8>)> = (0..20)
            .map(|i| (format!("f{i}"), vec![b'x'; 1000]))
            .collect();
        let refs: Vec<(&str, Option<&str>, &[u8])> = parts
            .iter()
            .map(|(n, d)| (n.as_str(), Some("a.bin"), d.as_slice()))
            .collect();
        let raw = body(&refs);
        let mut l = Limits::unlimited();
        l.max_file_size = 1000;
        l.max_total_size = raw.len();
        assert!(all(&raw, l).is_ok());
        assert_eq!(streamed_with(&raw, 777, l).unwrap().len(), 20);
        l.max_total_size = raw.len() - 1;
        assert_eq!(all(&raw, l).unwrap_err().status(), 413);
        let e = streamed_with(&raw, 777, l).unwrap_err();
        assert_eq!(e.status(), 413);
        assert_eq!(e.message(), "multipart body is too large");
        // 小さい本文でも途中で止まる（全部を読む前に失敗する）
        l.max_total_size = 5000;
        assert_eq!(streamed_with(&raw, 512, l).unwrap_err().status(), 413);
    }

    #[test]
    fn malformed_bodies_give_400() {
        let lf = format!("--{B}\nContent-Disposition: form-data; name=\"a\"\n\nx\n--{B}--\n");
        assert_eq!(
            all(lf.as_bytes(), Limits::unlimited())
                .unwrap_err()
                .status(),
            400
        );
        let mut raw = body(&[("a", None, b"x")]);
        raw.truncate(raw.len() - 8);
        assert_eq!(all(&raw, Limits::unlimited()).unwrap_err().status(), 400);
        assert_eq!(
            all(b"no boundary here", Limits::unlimited())
                .unwrap_err()
                .status(),
            400
        );
        let nodisp = format!("--{B}\r\nContent-Type: text/plain\r\n\r\nx\r\n--{B}--");
        assert_eq!(
            all(nodisp.as_bytes(), Limits::unlimited())
                .unwrap_err()
                .status(),
            400
        );
        // 空の本文（パート無し）は通る
        let empty = format!("--{B}--\r\n");
        assert!(all(empty.as_bytes(), Limits::unlimited())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn extractors() {
        let raw = body(&[
            ("name", None, b"veil"),
            ("age", None, b"3"),
            ("doc", Some("a.txt"), b"hi"),
            ("doc", Some("b.txt"), b"yo"),
        ]);
        let req = Request::new(
            "POST",
            "/u",
            vec![(
                Bytes::from_static(b"content-type"),
                Bytes::from(format!("multipart/form-data; boundary={}", B)),
            )],
            raw,
        );
        #[derive(serde::Deserialize)]
        struct P {
            name: String,
            age: u32,
        }
        let p: P = multipart_form(&req).unwrap();
        assert_eq!((p.name.as_str(), p.age), ("veil", 3));
        let f = upload_file(&req, "doc").unwrap();
        assert_eq!((f.file_name(), f.text().unwrap()), ("a.txt", "hi"));
        assert_eq!(upload_files(&req, "doc").unwrap().len(), 2);
        assert!(upload_file_opt(&req, "none").unwrap().is_none());
        assert_eq!(upload_file(&req, "none").unwrap_err().status_code(), 422);
        let p: P = super::super::extract::form_body(&req).unwrap();
        assert_eq!(p.age, 3);
    }

    #[test]
    fn temp_file_is_anonymous() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = temp_file_in(dir.path()).unwrap();
        std::io::Write::write_all(&mut f, b"abc").unwrap();
        // Unix は作った直後に消える。Windows は閉じたときに消える（FILE_FLAG_DELETE_ON_CLOSE）
        #[cfg(unix)]
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        drop(f);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn random_token_bodies_agree_buffered_and_streamed() {
        // 字句を乱数でつないだ本文で、溜めた本文と 2 つに分けた本文の結果が一致すること（fuzz_parse の assert）
        let toks: [&[u8]; 10] = [
            b"--XyZ",
            b"\r\n",
            b"Content-Disposition: form-data; name=\"a\"",
            b"Content-Disposition: form-data; name=\"f\"; filename=\"x\"",
            b"x",
            b"\r",
            b"-",
            b" ",
            b"\n",
            b"\r\n\r\n",
        ];
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..20000 {
            let n = (rnd() % 24) as usize;
            let mut d = vec![(rnd() % 256) as u8];
            for _ in 0..n {
                d.extend_from_slice(toks[(rnd() % toks.len() as u64) as usize]);
            }
            fuzz_parse(&d);
        }
    }

    #[test]
    fn fuzz_entry_point_agrees() {
        let mut d = vec![7u8];
        d.extend_from_slice(
            b"--XyZ\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n1\r\n--XyZ--",
        );
        fuzz_parse(&d);
        fuzz_parse(b"\x03--XyZ\r\n\r\n");
    }
}
