// F-264
//! Swagger UI for `/docs` (`[api] openapi_docs_ui`).
//!
//! - `"embedded"` (the default with the `openapi-ui-embedded` feature): serves swagger-ui-dist bundled in the library
//!   (`third_party/swagger-ui-dist`, gzip and brotli pre-compressed) from `<openapi_path>/assets/<version>/`, so `/docs`
//!   needs no network access. The page carries a `Content-Security-Policy` that only allows its own origin.
//! - `"cdn"`: loads swagger-ui-dist from `[api] openapi_docs_cdn_url` (a pinned jsDelivr URL by default).
//! - `"off"`: only `/openapi.json` is served.
//!
//! The HTML is built at startup and on every reload; the assets are `Bytes::from_static` (nothing is copied per request).

use super::app::ApiSettings;
#[cfg(feature = "openapi-ui-embedded")]
use super::handler::{self, HandlerEntry, OperationMeta};
#[cfg(feature = "openapi-ui-embedded")]
use super::request::Request;
#[cfg(feature = "openapi-ui-embedded")]
use super::response::Response;
use bytes::Bytes;
#[cfg(feature = "openapi-ui-embedded")]
use std::sync::Arc;
use std::sync::RwLock;

/// The swagger-ui-dist version used by the default CDN URL (the same as the bundled one).
pub(crate) const CDN_VERSION: &str = "5.34.0";

/// How `/docs` serves Swagger UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DocsUi {
    Embedded,
    Cdn(String),
    Off,
}

/// The name prefix of the built-in asset handlers (`__veil_openapi*` follows `[api] openapi_enabled`).
#[cfg(feature = "openapi-ui-embedded")]
const ASSET_HANDLER: &str = "__veil_openapi_asset_";

/// The mode chosen by the current settings (shared with the asset handlers, which answer 404 unless embedded).
static MODE: RwLock<Option<DocsUi>> = RwLock::new(None);

/// Reads `[api] openapi_docs_ui` and `[api] openapi_docs_cdn_url`.
pub(crate) fn mode(settings: &ApiSettings) -> Result<DocsUi, String> {
    let default = if cfg!(feature = "openapi-ui-embedded") {
        "embedded"
    } else {
        "cdn"
    };
    match settings.openapi_docs_ui.as_deref().unwrap_or(default) {
        "embedded" if cfg!(feature = "openapi-ui-embedded") => Ok(DocsUi::Embedded),
        "embedded" => Err(
            "[api] openapi_docs_ui = \"embedded\" needs the openapi-ui-embedded feature (use \"cdn\" or \"off\")"
                .into(),
        ),
        "cdn" => Ok(DocsUi::Cdn(
            settings
                .openapi_docs_cdn_url
                .clone()
                .unwrap_or_else(|| {
                    format!("https://cdn.jsdelivr.net/npm/swagger-ui-dist@{CDN_VERSION}")
                })
                .trim_end_matches('/')
                .to_string(),
        )),
        "off" => Ok(DocsUi::Off),
        other => Err(format!(
            "[api] openapi_docs_ui must be \"embedded\", \"cdn\" or \"off\" (got \"{other}\")"
        )),
    }
}

/// The `/docs` page for the settings (cloned per request: a reference count and a slice).
#[derive(Clone)]
pub(crate) struct DocsPage {
    pub html: Bytes,
    /// Headers added to the response.
    pub headers: &'static [(&'static str, &'static str)],
}

const EMBEDDED_HEADERS: &[(&str, &str)] = &[
    ("content-security-policy", EMBEDDED_CSP),
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "no-referrer"),
];

/// The Content-Security-Policy of the embedded page: scripts, styles and requests only from the same origin
/// (Swagger UI sets inline styles and uses `data:` images).
pub(crate) const EMBEDDED_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
img-src 'self' data:; font-src 'self' data:; connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'self'";

/// Builds the page and remembers the mode for the asset handlers (startup and every reload).
pub(crate) fn page(settings: &ApiSettings) -> Result<Option<DocsPage>, String> {
    let mode = mode(settings)?;
    let page = match &mode {
        DocsUi::Off => None,
        DocsUi::Embedded => Some(DocsPage {
            html: Bytes::from(embedded_html(settings)),
            headers: EMBEDDED_HEADERS,
        }),
        DocsUi::Cdn(base) => Some(DocsPage {
            html: Bytes::from(cdn_html(settings, base)),
            headers: &[],
        }),
    };
    *MODE.write().unwrap() = Some(mode);
    Ok(page)
}

#[cfg(feature = "openapi-ui-embedded")]
fn embedded() -> bool {
    matches!(*MODE.read().unwrap(), Some(DocsUi::Embedded))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `<openapi_path>/assets/<version>` (the version in the URL lets the assets be cached forever).
pub(crate) fn asset_base(settings: &ApiSettings) -> String {
    format!(
        "{}/assets/{}",
        settings.openapi_path.trim_end_matches('/'),
        CDN_VERSION
    )
}

// F-262・F-265: WebSocket の操作（x-veil-websocket）と gRPC の操作（x-grpc）はブラウザから試せないので Try it out を出さない Swagger UI の
// プラグイン。同梱の初期化のスクリプトと CDN のページの両方が使う（JS のコメントは入れない。初期化のスクリプトに
// `//` が無いことを試験で見ている）
macro_rules! no_try_it_out_plugin {
    () => {
        "const VeilNoTryItOut = () => ({\n  statePlugins: { spec: { wrapSelectors: {\n    allowTryItOutFor: (ori, system) => (state, path, method) =>\n      ori(state, path, method) &&\n      !system.specSelectors.specJson().getIn([\"paths\", path, method, \"x-veil-websocket\"]) &&\n      !system.specSelectors.specJson().getIn([\"paths\", path, method, \"x-grpc\"])\n  } } }\n});\n"
    };
}

/// The initializer is a same-origin script (no inline script, so the CSP needs no hash); it reads the URL of the
/// OpenAPI document from `data-url`. `validatorUrl: null` keeps the page from calling validator.swagger.io.
#[cfg(feature = "openapi-ui-embedded")]
pub(crate) const INITIALIZER_JS: &str = concat!(
    no_try_it_out_plugin!(),
    "window.ui = SwaggerUIBundle({\n  url: document.getElementById(\"swagger-ui\").dataset.url,\n  dom_id: \"#swagger-ui\",\n  deepLinking: true,\n  validatorUrl: null,\n  plugins: [VeilNoTryItOut]\n});\n"
);

fn embedded_html(settings: &ApiSettings) -> String {
    let title = html_escape(&settings.openapi_title);
    let url = html_escape(&settings.openapi_json_path);
    let base = html_escape(&asset_base(settings));
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} - Swagger UI</title>
<link rel="stylesheet" href="{base}/swagger-ui.css">
</head>
<body>
<div id="swagger-ui" data-url="{url}"></div>
<script src="{base}/swagger-ui-bundle.js"></script>
<script src="{base}/swagger-initializer.js"></script>
</body>
</html>
"##
    )
}

fn cdn_html(settings: &ApiSettings, base: &str) -> String {
    let title = html_escape(&settings.openapi_title);
    let url = serde_json::to_string(&settings.openapi_json_path)
        .unwrap_or_else(|_| "\"/openapi.json\"".into());
    let base = html_escape(base);
    let plugin = no_try_it_out_plugin!();
    format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} - Swagger UI</title>
<link rel="stylesheet" href="{base}/swagger-ui.css">
</head>
<body>
<div id="swagger-ui"></div>
<script src="{base}/swagger-ui-bundle.js" crossorigin></script>
<script>
{plugin}window.ui = SwaggerUIBundle({{ url: {url}, dom_id: "#swagger-ui", deepLinking: true, validatorUrl: null, plugins: [VeilNoTryItOut] }});
</script>
</body>
</html>
"##
    )
}

/// Registers the asset routes (the first time only; called next to the `/docs` route).
pub(crate) fn register_assets(settings: &ApiSettings) -> Result<(), String> {
    #[cfg(feature = "openapi-ui-embedded")]
    {
        let base = asset_base(settings);
        for a in assets::ALL.iter() {
            handler::register(HandlerEntry::new(
                format!("{ASSET_HANDLER}{}", a.name),
                &format!("{base}/{}", a.name),
                &["GET"],
                Arc::new(move |req: Request| {
                    let resp = if embedded() {
                        assets::respond(a, &req)
                    } else {
                        Response::not_found()
                    };
                    Box::pin(async move { resp })
                }),
                OperationMeta {
                    hidden: true,
                    ..Default::default()
                },
            )?)?;
        }
    }
    #[cfg(not(feature = "openapi-ui-embedded"))]
    let _ = settings;
    Ok(())
}

#[cfg(feature = "openapi-ui-embedded")]
mod assets {
    //! The bundled swagger-ui-dist (`third_party/swagger-ui-dist`, see its README.veil.md).
    use super::*;
    use std::sync::OnceLock;

    mod dist {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/third_party/swagger-ui-dist/assets.rs"
        ));
    }
    pub(super) use dist::VERSION;

    pub(crate) struct Asset {
        pub name: &'static str,
        content_type: &'static str,
        /// `Bytes` so that a response only bumps a reference count.
        etag: Bytes,
        /// Pre-compressed bodies (`None` for the small initializer, served as is).
        gz: Option<&'static [u8]>,
        br: Option<&'static [u8]>,
        /// The uncompressed body: static for the initializer, decompressed from `gz` on first use otherwise.
        plain: Option<&'static [u8]>,
        decoded: OnceLock<Bytes>,
    }

    /// ETag: the version and the head of the sha256 of the uncompressed file (the URL already carries the version).
    fn etag(sha: &str) -> Bytes {
        Bytes::from(format!("\"{}-{}\"", VERSION, &sha[..16]))
    }

    pub(crate) static ALL: std::sync::LazyLock<[Asset; 3]> = std::sync::LazyLock::new(|| {
        [
            Asset {
                name: "swagger-ui-bundle.js",
                content_type: "text/javascript; charset=utf-8",
                etag: etag(dist::BUNDLE_JS_SHA256),
                gz: Some(dist::BUNDLE_JS_GZ),
                br: Some(dist::BUNDLE_JS_BR),
                plain: None,
                decoded: OnceLock::new(),
            },
            Asset {
                name: "swagger-ui.css",
                content_type: "text/css; charset=utf-8",
                etag: etag(dist::CSS_SHA256),
                gz: Some(dist::CSS_GZ),
                br: Some(dist::CSS_BR),
                plain: None,
                decoded: OnceLock::new(),
            },
            Asset {
                name: "swagger-initializer.js",
                content_type: "text/javascript; charset=utf-8",
                etag: Bytes::from(format!("\"{VERSION}-init\"")),
                gz: None,
                br: None,
                plain: Some(INITIALIZER_JS.as_bytes()),
                decoded: OnceLock::new(),
            },
        ]
    });

    /// Whether `accept-encoding` accepts `coding` (a token without `q=0`).
    pub(crate) fn accepts(header: &str, coding: &str) -> bool {
        header.split(',').any(|part| {
            let mut it = part.split(';');
            let name = it.next().unwrap_or("").trim();
            if !name.eq_ignore_ascii_case(coding) {
                return false;
            }
            !it.any(|p| {
                let p = p.trim();
                p.strip_prefix("q=")
                    .or_else(|| p.strip_prefix("Q="))
                    .and_then(|q| q.trim().parse::<f32>().ok())
                    .is_some_and(|q| q == 0.0)
            })
        })
    }

    fn decoded(a: &Asset) -> Bytes {
        if let Some(p) = a.plain {
            return Bytes::from_static(p);
        }
        a.decoded
            .get_or_init(|| {
                use std::io::Read;
                let mut out = Vec::new();
                let gz = a.gz.unwrap_or_default();
                if flate2::read::GzDecoder::new(gz)
                    .read_to_end(&mut out)
                    .is_err()
                {
                    out.clear();
                }
                Bytes::from(out)
            })
            .clone()
    }

    pub(crate) fn respond(a: &'static Asset, req: &Request) -> Response {
        let cache = "public, max-age=31536000, immutable";
        if req.header("if-none-match").is_some_and(|v| {
            v.split(',')
                .any(|t| t.trim().as_bytes() == &a.etag[..] || t.trim() == "*")
        }) {
            return Response::new(304)
                .with_header("etag", a.etag.clone())
                .with_header("cache-control", cache);
        }
        let ae = req.header("accept-encoding").unwrap_or("");
        let (body, encoding) = match (a.br, a.gz) {
            (Some(br), _) if accepts(ae, "br") => (Bytes::from_static(br), Some("br")),
            (_, Some(gz)) if accepts(ae, "gzip") => (Bytes::from_static(gz), Some("gzip")),
            _ => (decoded(a), None),
        };
        let mut resp = Response::new(200)
            .with_header("content-type", a.content_type)
            .with_header("etag", a.etag.clone())
            .with_header("cache-control", cache)
            .with_header("x-content-type-options", "nosniff")
            .with_body(body);
        if a.gz.is_some() {
            resp = resp.with_header("vary", "accept-encoding");
        }
        if let Some(e) = encoding {
            resp = resp.with_header("content-encoding", e);
        }
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(ui: Option<&str>) -> ApiSettings {
        let mut s = ApiSettings::from_value(None);
        s.openapi_docs_ui = ui.map(str::to_string);
        s
    }

    #[test]
    fn modes() {
        assert_eq!(mode(&settings(Some("off"))), Ok(DocsUi::Off));
        assert_eq!(
            mode(&settings(Some("cdn"))),
            Ok(DocsUi::Cdn(format!(
                "https://cdn.jsdelivr.net/npm/swagger-ui-dist@{CDN_VERSION}"
            )))
        );
        assert!(mode(&settings(Some("bogus"))).is_err());
        let default = mode(&settings(None)).unwrap();
        if cfg!(feature = "openapi-ui-embedded") {
            assert_eq!(default, DocsUi::Embedded);
        } else {
            assert!(matches!(default, DocsUi::Cdn(_)));
            assert!(mode(&settings(Some("embedded"))).is_err());
        }
    }

    #[cfg(feature = "openapi-ui-embedded")]
    #[test]
    fn embedded_page_has_no_external_reference() {
        let p = page(&settings(None)).unwrap().unwrap();
        let html = std::str::from_utf8(&p.html).unwrap();
        for needle in ["http://", "https://", "//cdn", "\"//", "'//", "<script>"] {
            assert!(!html.contains(needle), "{needle} in {html}");
        }
        assert!(html.contains("/docs/assets/5.34.0/swagger-ui-bundle.js"));
        assert!(html.contains("data-url=\"/openapi.json\""));
        assert!(p
            .headers
            .iter()
            .any(|(n, v)| *n == "content-security-policy" && v.contains("script-src 'self';")));
        // The initializer must not reach out either.
        assert!(!INITIALIZER_JS.contains("//"));
    }

    // F-262: WebSocket の操作では Try it out を出さない（同梱・CDN の両方）
    #[test]
    fn websocket_operations_hide_try_it_out() {
        let cdn = page(&settings(Some("cdn"))).unwrap().unwrap();
        let cdn = std::str::from_utf8(&cdn.html).unwrap();
        assert!(cdn.contains("x-veil-websocket") && cdn.contains("plugins: [VeilNoTryItOut]"));
        assert!(cdn.contains("x-grpc"));
        #[cfg(feature = "openapi-ui-embedded")]
        assert!(
            INITIALIZER_JS.contains("x-veil-websocket")
                && INITIALIZER_JS.contains("plugins: [VeilNoTryItOut]")
        );
    }

    #[cfg(feature = "openapi-ui-embedded")]
    #[test]
    fn bundled_version_matches_cdn_default() {
        assert_eq!(assets::VERSION, CDN_VERSION);
    }

    #[cfg(feature = "openapi-ui-embedded")]
    #[test]
    fn accept_encoding_parsing() {
        assert!(assets::accepts("gzip, br", "br"));
        assert!(assets::accepts("gzip;q=0.5, BR;q=1", "br"));
        assert!(!assets::accepts("gzip, br;q=0", "br"));
        assert!(!assets::accepts("identity", "gzip"));
        assert!(!assets::accepts("", "gzip"));
    }

    #[cfg(feature = "openapi-ui-embedded")]
    #[test]
    fn assets_decode_and_match_their_sha256() {
        #[cfg(not(any(target_os = "openbsd", target_os = "netbsd")))]
        use aws_lc_rs::digest;
        #[cfg(any(target_os = "openbsd", target_os = "netbsd"))]
        use ring::digest;
        let hex = |b: &[u8]| -> String { b.iter().map(|x| format!("{x:02x}")).collect() };
        let readme = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/third_party/swagger-ui-dist/README.veil.md"
        ));
        *MODE.write().unwrap() = Some(DocsUi::Embedded);
        for a in assets::ALL.iter() {
            let req = |n: &'static str, v: &str| {
                Request::new(
                    "GET",
                    "/",
                    vec![(
                        Bytes::from_static(n.as_bytes()),
                        Bytes::copy_from_slice(v.as_bytes()),
                    )],
                    Bytes::new(),
                )
            };
            let plain = assets::respond(a, &req("accept-encoding", "identity"));
            assert_eq!(plain.status_code(), 200);
            assert!(plain.header("content-encoding").is_none());
            if a.name != "swagger-initializer.js" {
                let h = hex(digest::digest(&digest::SHA256, plain.body()).as_ref());
                assert!(
                    readme.contains(&h),
                    "{} sha256 {h} not in README.veil.md",
                    a.name
                );
                let gz = assets::respond(a, &req("accept-encoding", "gzip, deflate"));
                assert_eq!(gz.header("content-encoding"), Some(&b"gzip"[..]));
                assert_eq!(gz.header("vary"), Some(&b"accept-encoding"[..]));
                let br = assets::respond(a, &req("accept-encoding", "gzip, br"));
                assert_eq!(br.header("content-encoding"), Some(&b"br"[..]));
                assert!(br.body().len() < gz.body().len());
                assert!(!String::from_utf8_lossy(plain.body()).contains("sourceMappingURL="));
            }
            let etag = std::str::from_utf8(plain.header("etag").unwrap())
                .unwrap()
                .to_string();
            let not_modified = assets::respond(a, &req("if-none-match", &etag));
            assert_eq!(not_modified.status_code(), 304);
            assert!(not_modified.body().is_empty());
        }
    }
}
