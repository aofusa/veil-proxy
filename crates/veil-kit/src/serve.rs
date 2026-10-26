// F-267: 静的ファイル。ディレクトリは本体の File バックエンド（App::serve_dir）、ハンドラから返す 1 ファイルは
// offload で読む（小さなファイル向け）。
//! Static files.
//!
//! Directories are served by veil's own file server, outside the handler path (sendfile, kTLS, Range, ETag,
//! precompressed files and the file caches all apply):
//!
//! ```no_run
//! use veil::prelude::*;
//! App::new().with(veil_kit::serve::dir("/assets", "/srv/www/assets")).run().unwrap();
//! ```
//!
//! [`file`] returns one file from a handler (for small files that a handler picks, such as downloads behind
//! authentication).

pub use veil::api::ServeOptions;
use veil::api::{App, Response};

/// A plugin that serves `dir` under `prefix` with the default [`ServeOptions`] (file metadata and contents caches on).
pub fn dir(prefix: &str, dir: &str) -> impl FnOnce(App) -> App {
    dir_with(prefix, dir, ServeOptions::default())
}

/// Like [`dir`] with options.
pub fn dir_with(prefix: &str, dir: &str, options: ServeOptions) -> impl FnOnce(App) -> App {
    let (prefix, dir) = (prefix.to_string(), dir.to_string());
    move |app: App| app.serve_dir(&prefix, &dir, options)
}

/// [`ServeOptions`] with `index` set (served for directory requests).
pub fn with_index(index: &str) -> ServeOptions {
    ServeOptions {
        index: Some(index.to_string()),
        ..ServeOptions::default()
    }
}

/// The media type for a file name (by extension; `application/octet-stream` when unknown).
pub fn content_type(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match ext.to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

/// Reads `path` on the file offload pool and returns it (404 when it does not exist, 413 when it is larger
/// than `max_size`). `path` must not come from the request unchecked; use [`dir`] to serve a directory.
pub async fn file(path: impl Into<std::path::PathBuf>, max_size: u64) -> Response {
    let path = path.into();
    let name = path.to_string_lossy().into_owned();
    // 理由: offload の閉包の中（ワーカーのスレッドではない）なのでブロッキングの fs を使ってよい
    #[allow(clippy::disallowed_methods)]
    let r = veil::runtime::offload::offload(move || -> std::io::Result<Option<Vec<u8>>> {
        let meta = std::fs::metadata(&path)?;
        if !meta.is_file() {
            return Err(std::io::ErrorKind::NotFound.into());
        }
        if meta.len() > max_size {
            return Ok(None);
        }
        std::fs::read(&path).map(Some)
    })
    .await;
    match r {
        Ok(Some(data)) => {
            let mut r = Response::bytes(data);
            r.set_header("content-type", content_type(&name));
            r
        }
        Ok(None) => Response::error(413, "Payload Too Large"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Response::not_found(),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            Response::error(403, "Forbidden")
        }
        Err(e) => {
            ftlog::error!("reading {} failed: {}", name, e);
            Response::error(500, "Internal Server Error")
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn content_types() {
        assert_eq!(super::content_type("a/b.HTML"), "text/html; charset=utf-8");
        assert_eq!(super::content_type("x.woff2"), "font/woff2");
        assert_eq!(super::content_type("noext"), "application/octet-stream");
    }
}
