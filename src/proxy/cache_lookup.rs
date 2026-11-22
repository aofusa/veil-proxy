//! プロキシのキャッシュの引き当てと、当たりの応答のヘッダ（HTTP/1.1・HTTP/2・HTTP/3 で共有する。F-257）
//!
//! 以前は HTTP/1.1 のクライアントの要求（`proxy::forward`）だけがキャッシュを読み書きし、HTTP/2・HTTP/3 の中継は
//! キャッシュを見ずに上流へ送っていた。要求ヘッダの型はプロトコルごとに違うので、ここでは (名前, 値) の組を返す
//! イテレータで受ける（`Clone` で何度も読む。写しは作らない）。

use super::*;

/// キャッシュの引き当ての結果
pub(crate) enum CacheLookup {
    /// このルート・要求ではキャッシュを使わない（無効・対象外のメソッド・除外のパス・`Authorization` 等）
    Bypass,
    /// 外れ。上流の応答を取り込んで保存する
    Miss(Box<CacheSaveContext>),
    /// 当たり
    Hit(CacheHit),
}

/// キャッシュの当たり
pub(crate) struct CacheHit {
    pub(crate) entry: Arc<cache::CacheEntry>,
    /// 期限切れ（stale-while-revalidate の猶予の間）。裏で取り直す
    pub(crate) stale: bool,
    /// 条件付き要求（`If-None-Match`・`If-Modified-Since`）が一致した（本文の無い 304 を返す）
    pub(crate) not_modified: bool,
    pub(crate) key: cache::CacheKey,
}

/// `[route.cache]` のルートで要求をキャッシュに引き当てる。`host` は `Host`（HTTP/2・HTTP/3 は `:authority`）、
/// `target` は要求の対象（パスとクエリ）、`headers` は要求ヘッダの (名前, 値) の組。当たり・外れを数える
/// （`veil_proxy_cache_hits_total`・`veil_proxy_cache_misses_total`）。
///
/// `host`・`target` はバイト列で受け、キャッシュのあるルートでだけ UTF-8 として読む（キャッシュの無いルートの要求では
/// 読まない）。UTF-8 でない要求はキャッシュを使わない（置き換えた文字列のキーでほかの要求の応答と混ぜない）
pub(crate) fn lookup<'h, I>(
    cfg: &Arc<cache::CacheConfig>,
    method: &[u8],
    host: &[u8],
    target: &[u8],
    headers: I,
) -> CacheLookup
where
    I: Iterator<Item = (&'h [u8], &'h [u8])> + Clone,
{
    if !cfg.enabled {
        return CacheLookup::Bypass;
    }
    let (Ok(host), Ok(target)) = (std::str::from_utf8(host), std::str::from_utf8(target)) else {
        return CacheLookup::Bypass;
    };
    let Some(key) = request_key(cfg, method, host, target, headers.clone()) else {
        return CacheLookup::Bypass;
    };
    let Some(manager) = cache::get_global_cache() else {
        return CacheLookup::Bypass;
    };
    let found = match manager.get(&key) {
        Some(entry) => Some((entry, false)),
        // 期限切れでも stale-while-revalidate の猶予（60 秒）の間は返す
        None if cfg.stale_while_revalidate => manager
            .get_stale(&key, cache::STALE_WHILE_REVALIDATE_SECS)
            .map(|e| (e, true)),
        None => None,
    };
    match found {
        Some((entry, stale)) => {
            debug!(
                "Cache {} for {} {}",
                if stale { "STALE" } else { "HIT" },
                host,
                target
            );
            record_cache_hit(host);
            let not_modified = cfg.enable_etag && conditional_matches(&entry, headers);
            CacheLookup::Hit(CacheHit {
                entry,
                stale,
                not_modified,
                key,
            })
        }
        None => {
            debug!("Cache MISS for {} {}", host, target);
            record_cache_miss(host);
            CacheLookup::Miss(Box::new(save_context(cfg, key, host, headers)))
        }
    }
}

/// 当たりの本文を読めなかった（ディスクの読み込みの失敗）。エントリを消し、上流から取り直して保存し直す
/// （`host` は [`lookup`] へ渡したもの。当たりは UTF-8 の `host` の要求でだけ起きる）
pub(crate) fn refill<'h, I>(
    cfg: &Arc<cache::CacheConfig>,
    hit: CacheHit,
    host: &[u8],
    headers: I,
) -> Box<CacheSaveContext>
where
    I: Iterator<Item = (&'h [u8], &'h [u8])> + Clone,
{
    if let Some(manager) = cache::get_global_cache() {
        manager.invalidate(&hit.key);
    }
    let host = std::str::from_utf8(host).unwrap_or_default();
    Box::new(save_context(cfg, hit.key, host, headers))
}

/// 要求のキャッシュのキー（キャッシュを使わない要求は `None`）
fn request_key<'h, I>(
    cfg: &cache::CacheConfig,
    method: &[u8],
    host: &str,
    target: &str,
    headers: I,
) -> Option<cache::CacheKey>
where
    I: Iterator<Item = (&'h [u8], &'h [u8])> + Clone,
{
    if !cfg.is_cacheable_method(method)
        || cfg.should_bypass(target)
        || cfg.bypass_for_authorization(headers.clone())
        // B-425: キーに挙げたヘッダが 2 つ以上・UTF-8 でない要求はキャッシュを使わない
        || !cache_key_headers_usable(headers.clone(), &cfg.key_headers)
    {
        return None;
    }
    let (path, query) = match target.find('?') {
        Some(i) => (&target[..i], Some(&target[i + 1..])),
        None => (target, None),
    };
    // key_headers の値をキーに含める（保存も同じキー）
    let vary = if cfg.key_headers.is_empty() {
        None
    } else {
        let v = extract_vary_headers_for_cache_key(headers, &cfg.key_headers);
        (!v.is_empty()).then_some(v)
    };
    cache::CacheKey::from_request(
        method,
        host,
        path,
        query,
        cfg.include_query,
        vary.as_deref(),
        cfg.route_ns,
    )
}

/// 外れた要求の応答を取り込むコンテキスト
fn save_context<'h, I>(
    cfg: &Arc<cache::CacheConfig>,
    key: cache::CacheKey,
    host: &str,
    headers: I,
) -> CacheSaveContext
where
    I: Iterator<Item = (&'h [u8], &'h [u8])> + Clone,
{
    // 取り込む本文の上限（最大 10MB）
    let max_capture = cfg.max_memory_size.min(10 * 1024 * 1024);
    let mut ctx = CacheSaveContext::new(key, cfg.clone(), host.to_string(), max_capture);
    // B-321: Cookie の付いた要求への応答は、共有を明示したものだけ保存する（キーに Cookie があれば別）
    ctx.request_has_cookie = headers
        .clone()
        .any(|(n, _)| n.eq_ignore_ascii_case(b"cookie"))
        && !cfg
            .key_headers
            .iter()
            .any(|k| k.eq_ignore_ascii_case("cookie"));
    ctx
}

/// 条件付き要求がキャッシュの応答に一致するか（304 を返すか）。`If-None-Match` があれば `If-Modified-Since` は見ない
/// （RFC 9110 §13.1.3。以前は `If-None-Match` が外れると `If-Modified-Since` で 304 を返し得た）
fn conditional_matches<'h, I>(entry: &cache::CacheEntry, headers: I) -> bool
where
    I: Iterator<Item = (&'h [u8], &'h [u8])> + Clone,
{
    let find = |name: &[u8]| {
        headers
            .clone()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| std::str::from_utf8(v).unwrap_or(""))
    };
    if let Some(inm) = find(b"if-none-match") {
        return entry
            .etag
            .as_deref()
            .is_some_and(|etag| etag_matches(inm, etag));
    }
    find(b"if-modified-since").is_some_and(|ims| {
        entry
            .last_modified
            .as_deref()
            .is_some_and(|lm| last_modified_matches(ims, lm))
    })
}

/// 304 へ載せる保存したヘッダ（条件付き要求への応答に要るもの）
#[inline]
fn kept_in_304(name: &[u8]) -> bool {
    name.eq_ignore_ascii_case(b"etag")
        || name.eq_ignore_ascii_case(b"last-modified")
        || name.eq_ignore_ascii_case(b"cache-control")
        || name.eq_ignore_ascii_case(b"vary")
        || name.eq_ignore_ascii_case(b"content-location")
}

/// 当たりの応答のステータス
#[inline]
pub(crate) fn hit_status(hit: &CacheHit) -> u16 {
    if hit.not_modified {
        304
    } else {
        hit.entry.status_code
    }
}

#[inline]
fn x_cache_value(hit: &CacheHit) -> &'static [u8] {
    if hit.stale {
        b"STALE"
    } else {
        b"HIT"
    }
}

/// 保存したヘッダのうち当たりの応答へ載せるもの（304 は [`kept_in_304`] だけ。`remove_response_headers` と、自分の
/// `server` を付ける応答では上流の `server` を除く）
#[inline]
fn hit_keeps(
    hit: &CacheHit,
    security: Option<&SecurityConfig>,
    own_server: bool,
    name: &[u8],
) -> bool {
    !(hit.not_modified && !kept_in_304(name))
        && !security.is_some_and(|s| s.removes_response_header(name))
        && !(own_server && name.eq_ignore_ascii_case(b"server"))
}

/// 当たりの応答のヘッダ（HTTP/2・HTTP/3）を `out` の後ろへ足す。保存したヘッダにルートの `remove_response_headers`・
/// `add_response_headers` を当てて `x-cache` を付ける（上流から中継する応答と同じ方針。Proxy-Wasm の応答の段は通らない。
/// ヘッダは写さない）。`security` が `None` なら方針は当てない（HTTP/3 のストリーミングの経路はメインループが当てる）。
/// `own_server` は自分の `server` を付ける応答（保存した上流の `server` を落とす）
#[cfg(any(feature = "http2", feature = "http3", test))]
pub(crate) fn push_hit_headers(
    out: &mut Vec<(Bytes, Bytes)>,
    hit: &CacheHit,
    security: Option<&SecurityConfig>,
    own_server: bool,
) {
    for (name, value) in hit.entry.headers.iter() {
        if hit_keeps(hit, security, own_server, name) {
            out.push((name.clone(), value.clone()));
        }
    }
    if let Some(security) = security {
        out.extend(security.add_response_header_bytes().iter().cloned());
    }
    out.push((
        Bytes::from_static(b"x-cache"),
        Bytes::from_static(x_cache_value(hit)),
    ));
}

/// 当たりの応答の HTTP/1.1 のヘッダ（ステータス行から空行まで）。上流から中継する応答（`render_response_head`）と
/// 同じく、自分の `Server`・`remove_response_headers`・`add_response_headers`・`Alt-Svc`・CORS を当て、`X-Cache` と
/// `Connection` を付ける（以前は保存した上流のヘッダをそのまま書き、方針を当てていなかった）
pub(crate) fn render_h1_hit_head(
    hit: &CacheHit,
    security: &SecurityConfig,
    close: bool,
) -> Vec<u8> {
    let status = hit_status(hit);
    let mut out = Vec::with_capacity(512);
    out.extend_from_slice(b"HTTP/1.1 ");
    let mut code = itoa::Buffer::new();
    out.extend_from_slice(code.format(status).as_bytes());
    out.push(b' ');
    out.extend_from_slice(status_code_to_reason(status).as_bytes());
    out.extend_from_slice(b"\r\n");
    let own_server = crate::pool::append_own_server_line(&mut out, security);
    for (name, value) in hit.entry.headers.iter() {
        if hit_keeps(hit, Some(security), own_server, name) {
            out.extend_from_slice(name);
            out.extend_from_slice(b": ");
            out.extend_from_slice(value);
            out.extend_from_slice(b"\r\n");
        }
    }
    for (name, value) in &security.add_response_headers {
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    crate::pool::append_own_alt_svc_line(&mut out, security);
    out.extend_from_slice(b"X-Cache: ");
    out.extend_from_slice(x_cache_value(hit));
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(if close {
        b"Connection: close\r\n\r\n".as_slice()
    } else {
        b"Connection: keep-alive\r\n\r\n".as_slice()
    });
    // `[route.traffic.cors]` の実際の応答のヘッダ（F-273）
    crate::route_cors::apply_h1(&mut out, security);
    out
}

/// stale-if-error（`[route.cache] stale_if_error`）: キャッシュの外れの要求に上流から使える応答を得られなかったとき
/// （veil が返す 502・504、上流の 502・504）に返す期限切れの保存した応答（期限から 1 時間まで）。
///
/// B-492: 以前は HTTP/1.1 のクライアントだけが、エラーページ（上流の 502・504 なら中継した応答）を書いた後で
/// この応答を同じ接続へ続けて書いていた（応答が 2 つになる）。HTTP/2・HTTP/3 では返していなかった
pub(crate) fn stale_fallback(ctx: Option<&CacheSaveContext>) -> Option<CacheHit> {
    #[cfg(feature = "cache")]
    {
        let ctx = ctx?;
        if !ctx.route.stale_if_error {
            return None;
        }
        let entry = cache::get_global_cache()?.get_stale(&ctx.key, cache::STALE_IF_ERROR_SECS)?;
        debug!("stale-if-error: serving a stale response for {}", ctx.host);
        Some(CacheHit {
            entry,
            stale: true,
            not_modified: false,
            key: ctx.key.clone(),
        })
    }
    #[cfg(not(feature = "cache"))]
    {
        let _ = ctx;
        None
    }
}

/// stale-if-error で保存した応答に替える上流の応答のステータスか（502・504。B-492）
#[inline]
pub(crate) fn is_gateway_error(status: u16) -> bool {
    matches!(status, 502 | 504)
}

/// 中継する本文（断片・溜めた全体）を保存のために取り込み、そのまま返す（`ctx` が `None` なら何もしない）。
///
/// 値で受けて返す。本文の局所変数を `&chunk` で借りてから送り先へ移して `await` すると、借りた局所変数は移した後も
/// スコープの終わりまでフューチャの状態に置き場が残る（HTTP/2 の chunked の中継では `Bytes` の 32 バイトと、
/// 落とすかどうかの印）
#[cfg(any(feature = "http2", feature = "http3"))]
#[inline]
pub(crate) fn capture<B: AsRef<[u8]>>(ctx: Option<&mut CacheSaveContext>, chunk: B) -> B {
    if let Some(ctx) = ctx {
        ctx.append_body(chunk.as_ref());
    }
    chunk
}

/// 当たりの応答を HTTP/1.1 のクライアントへ書く（本文はメモリのエントリなら写さずに、ヘッダと 1 回の書き込み
/// （平文は SENDMSG）で送る）。書けたら (ステータス, 書いたバイト数)。`close` は送った後に閉じる応答
pub(crate) async fn write_h1_hit(
    client_stream: &mut ServerTls,
    hit: &CacheHit,
    body: Bytes,
    security: &SecurityConfig,
    close: bool,
) -> Option<(u16, u64)> {
    let head = render_h1_hit_head(hit, security, close);
    let status = hit_status(hit);
    let len = (head.len() + body.len()) as u64;
    let written = if body.is_empty() {
        matches!(
            timeout(WRITE_TIMEOUT, client_stream.write_all(head)).await,
            Ok((Ok(_), _))
        )
    } else {
        matches!(
            timeout(WRITE_TIMEOUT, client_stream.write_all_vectored(head, body)).await,
            Ok((Ok(()), _, _))
        )
    };
    written.then_some((status, len))
}

/// 上流が 502・504 を返したとき、stale-if-error の保存した応答に替えるならその当たり（同期の判定だけ。普通の応答では
/// フューチャを作らない。B-492）
#[inline]
pub(crate) fn gateway_error_stale(
    status: u16,
    cache_ctx: Option<&CacheSaveContext>,
) -> Option<CacheHit> {
    if !is_gateway_error(status) {
        return None;
    }
    stale_fallback(cache_ctx)
}

/// [`gateway_error_stale`] の当たりを HTTP/1.1 のクライアントへ上流のエラーの代わりに書く（上流の応答はまだ何も中継して
/// いない）。戻り値は (送ったバイト数, ステータス, 上流の接続を戻すか（常に偽）, 閉じるか)。本文を読めなければ `None`
/// （呼び出し側は上流のエラーをそのまま中継する）。
///
/// 箱に入れたフューチャを返す（エラーの経路から呼ぶので、箱に入れないと普通の応答を中継するフューチャの状態にも
/// その大きさが載る。作るのは保存した応答を返すときだけ）
pub(crate) fn write_h1_stale<'a>(
    client_stream: &'a mut ServerTls,
    hit: CacheHit,
    status: u16,
    security: &'a SecurityConfig,
    close: bool,
) -> std::pin::Pin<Box<impl std::future::Future<Output = Option<(u64, u16, bool, bool)>> + 'a>> {
    Box::pin(async move {
        let body = hit_body(&hit).await?;
        Some(
            match write_h1_hit(client_stream, &hit, body, security, close).await {
                Some((st, len)) => (len, st, false, false),
                None => (0, status, false, true),
            },
        )
    })
}

/// 当たりの応答の本文。メモリのエントリは写さずに返し、ディスクのエントリは offload で読む（読めなければ `None`）。
/// 304 は空
pub(crate) async fn hit_body(hit: &CacheHit) -> Option<Bytes> {
    if hit.not_modified {
        return Some(Bytes::new());
    }
    if let Some(body) = hit.entry.memory_body() {
        return Some(body.clone());
    }
    let path = hit.entry.disk_path()?.to_path_buf();
    // 理由付き allow: 同期 FS は offload 閉包内（専用ワーカースレッド）で実行され、イベントループを塞がない。
    #[allow(clippy::disallowed_methods)]
    match crate::runtime::offload::offload(move || std::fs::read(path)).await {
        Ok(data) => Some(Bytes::from(data)),
        Err(e) => {
            error!("Failed to read disk cache file: {}", e);
            None
        }
    }
}

#[cfg(all(test, feature = "cache"))]
mod tests {
    use super::*;

    fn pairs<'a>(
        h: &'a [(&'a str, &'a str)],
    ) -> impl Iterator<Item = (&'a [u8], &'a [u8])> + Clone {
        h.iter().map(|(n, v)| (n.as_bytes(), v.as_bytes()))
    }

    fn entry(headers: &[(&str, &str)]) -> cache::CacheEntry {
        cache::CacheEntry::new(
            200,
            headers
                .iter()
                .map(|(n, v)| (Box::from(n.as_bytes()), Box::from(v.as_bytes())))
                .collect(),
            cache::CacheStorage::Memory(Bytes::from_static(b"x")),
            60,
        )
    }

    /// `If-None-Match` があれば `If-Modified-Since` は見ない（RFC 9110 §13.1.3）
    #[test]
    fn if_none_match_takes_precedence() {
        let e = entry(&[
            ("etag", "\"a\""),
            ("last-modified", "Mon, 01 Jan 2024 00:00:00 GMT"),
        ]);
        let lm = "Mon, 01 Jan 2024 00:00:00 GMT";
        assert!(conditional_matches(
            &e,
            pairs(&[("if-none-match", "\"a\"")])
        ));
        assert!(conditional_matches(&e, pairs(&[("if-modified-since", lm)])));
        assert!(!conditional_matches(
            &e,
            pairs(&[("if-none-match", "\"b\""), ("if-modified-since", lm)])
        ));
        assert!(!conditional_matches(&e, pairs(&[("accept", "*/*")])));
    }

    /// 304 には条件付き要求への応答に要るヘッダだけを載せ、`remove_response_headers`・上流の `server` を落とす
    #[test]
    fn hit_headers_follow_route_policy() {
        let security = SecurityConfig {
            remove_response_headers: vec!["x-internal".to_string()],
            add_response_headers: [("X-Added".to_string(), "1".to_string())]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let e = Arc::new(entry(&[
            ("content-type", "text/plain"),
            ("etag", "\"a\""),
            ("x-internal", "secret"),
            ("server", "upstream"),
            ("content-length", "1"),
        ]));
        let hit = |not_modified| CacheHit {
            entry: e.clone(),
            stale: false,
            not_modified,
            key: cache::CacheKey::from_request(b"GET", "h", "/", None, true, None, 0).unwrap(),
        };
        let names = |h: &CacheHit| {
            let mut out = Vec::new();
            push_hit_headers(&mut out, h, Some(&security), true);
            out.iter()
                .map(|(n, v)| {
                    format!(
                        "{}={}",
                        String::from_utf8_lossy(n),
                        String::from_utf8_lossy(v)
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(&hit(false)),
            [
                "content-type=text/plain",
                "etag=\"a\"",
                "content-length=1",
                "x-added=1",
                "x-cache=HIT"
            ]
        );
        assert_eq!(
            names(&hit(true)),
            ["etag=\"a\"", "x-added=1", "x-cache=HIT"]
        );
        let head = String::from_utf8(render_h1_hit_head(&hit(true), &security, true)).unwrap();
        assert!(head.starts_with("HTTP/1.1 304 Not Modified\r\n"), "{head}");
        assert!(
            !head.contains("x-internal") && !head.contains("content-type"),
            "{head}"
        );
        assert!(
            head.ends_with("X-Cache: HIT\r\nConnection: close\r\n\r\n"),
            "{head}"
        );
    }
}
