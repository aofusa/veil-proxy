//! コマンドライン引数・HTTP から HTTPS へのリダイレクト
//!
//! F-233 で `src/config.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// コマンドライン引数パース
// ====================

// ====================
// HTTP to HTTPS リダイレクトハンドラー
// ====================
//
// HTTPアクセスをHTTPSにリダイレクトするための軽量ハンドラー。
// セキュリティ上の理由から、HTTPではリダイレクトのみを行い、
// コンテンツは一切配信しません。
//
// 301 Moved Permanently を使用することで、ブラウザがリダイレクト先を
// キャッシュし、以降のアクセスでは直接HTTPSに接続します。

// HTTP_301_REDIRECT_TEMPLATE, HTTP_301_REDIRECT_SUFFIX は
// crate::constants モジュールに移動しました

/// HTTPS へのリダイレクト先 URL（F-200）。ポートは `[server].listen` のもの（443 なら省略）。
pub fn https_redirect_url(host: &str, path: &str) -> String {
    let port = HTTPS_REDIRECT_PORT.load(std::sync::atomic::Ordering::Relaxed);
    // Host からポートを外す（`[v6]:port` は `]` まで残す）
    let host = match host.find(']') {
        Some(i) if host.starts_with('[') => &host[..=i],
        _ => host.split(':').next().unwrap_or(host),
    };
    let host = if host.is_empty() { "localhost" } else { host };
    let path = if path.starts_with('/') { path } else { "/" };
    if port == 443 {
        format!("https://{}{}", host, path)
    } else {
        format!("https://{}:{}{}", host, port, path)
    }
}

/// 平文リスナー（`listen_cleartext`、`enable_secure_redirect = true`）の HTTP/1.1 を https へ 301 で返す。
///
/// `initial` はプロトコル検出で既に読んだ先頭バイト列。要求ヘッダが揃うまで（4KiB まで）読み足す。
pub async fn handle_http_redirect(mut stream: TcpStream, initial: Vec<u8>) {
    let mut buffer = initial;
    while !buffer.windows(4).any(|w| w == b"\r\n\r\n") && buffer.len() < 4096 {
        let read_result = timeout(Duration::from_secs(5), stream.read(vec![0u8; 4096])).await;
        match read_result {
            Ok((Ok(n), buf)) if n > 0 => buffer.extend_from_slice(&buf[..n]),
            _ => break,
        }
    }
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut req = Request::new(&mut headers);
    let path = match req.parse(&buffer) {
        Ok(Status::Complete(_)) | Ok(Status::Partial) => req.path.unwrap_or("/"),
        Err(_) => "/",
    };
    let host = req
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("Host"))
        .map(|h| std::str::from_utf8(h.value).unwrap_or(""))
        .unwrap_or("");
    let redirect_url = https_redirect_url(host, path);

    // 301レスポンスを構築
    let mut response = Vec::with_capacity(
        HTTP_301_REDIRECT_TEMPLATE.len() + redirect_url.len() + HTTP_301_REDIRECT_SUFFIX.len(),
    );
    response.extend_from_slice(HTTP_301_REDIRECT_TEMPLATE);
    response.extend_from_slice(redirect_url.as_bytes());
    response.extend_from_slice(HTTP_301_REDIRECT_SUFFIX);

    // レスポンスを送信
    let _ = timeout(Duration::from_secs(5), stream.write_all(response)).await;
}

/// High-Performance Reverse Proxy Server
///
/// io_uring（独自ランタイム）と rustls を使用した高性能リバースプロキシサーバー
#[derive(Parser, Debug)]
#[command(name = "veil")]
#[command(author, version, about, long_about = None)]
pub struct CliArgs {
    /// 設定ファイルのパス
    #[arg(short, long, default_value = DEFAULT_CONFIG_PATH)]
    pub config: PathBuf,

    /// 設定ファイルの構文と内容を検証して終了（nginx -t 相当）
    #[arg(short = 't', long = "test")]
    pub test_config: bool,

    /// config.toml の値をコマンドラインから上書きする（繰り返し指定可）。
    ///
    /// 構文: `<path> = <toml-value>`（`=` 前後の空白は任意）。
    /// - `<path>` はドット区切りのキーパス（例: `server.threads`）。セグメントは
    ///   裸のキー（`[A-Za-z0-9_-]+`）・クオート文字列（`"a.b"`/`'a.b'`、キーに
    ///   ドットを含む場合用）・10 進数の配列インデックス（親が配列の場合。
    ///   例: `l4.0.listen`）のいずれか。セクション記法風に `[server].threads = 1`
    ///   のように先頭（または任意のセグメント）を `[ ]` で囲んでもよい。
    /// - `<toml-value>` は TOML 値としてパースする（`1`、`"str"`、`true`、`1.5`、
    ///   `[1, 2]`、`{ a = 1 }` 等）。クオート・角括弧・波括弧のいずれも含まない
    ///   裸のトークンが TOML 値として不正な場合は文字列として扱う
    ///   （例: `-o "tls.cert_path = /etc/veil/cert.pem"`）。
    /// - 例: `-o "server.threads = 1" -o '[server].threads = 1'`
    ///
    /// 起動時のみならず SIGHUP によるホットリロード・`-t` 検証にも同じ内容が適用される。
    #[arg(short = 'o', long = "override", value_name = "KEY=VALUE")]
    pub overrides: Vec<String>,
}

/// 設定ファイルを検証（読み込みとバリデーションのみ、起動しない）
///
/// nginx -t 相当の機能を提供します。
/// - TOML構文のパース
/// - 設定値のバリデーション
/// - TLS証明書・秘密鍵の存在確認
// 理由付き allow: 起動・リロード・設定検証時のみ実行されるコールドパス（データプレーン非経由）。
#[allow(clippy::disallowed_methods)]
pub fn test_config_file(path: &Path) -> io::Result<()> {
    // ファイル存在確認
    // API ライブラリのモードではファイルが無くてもよい（既定設定のみで起動する）。
    #[cfg(feature = "handler")]
    let file_required = !crate::config_override::library_mode();
    #[cfg(not(feature = "handler"))]
    let file_required = true;
    if file_required && !path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("configuration file not found: {}", path.display()),
        ));
    }

    // TOMLパース（--override 適用込み。src/config_override.rs の単一チョークポイント）
    let config_str = read_config_text(path)?;
    let config: Config = crate::config_override::apply_to_toml_str(&config_str)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    // 設定バリデーション
    validate_config(&config)?;

    // TLS証明書の存在確認（平文専用サーバで `[tls]` が無い場合は対象外）
    if !config.tls.is_configured() {
        return Ok(());
    }
    let cert_path = Path::new(&config.tls.cert_path);
    if !cert_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("TLS certificate not found: {}", config.tls.cert_path),
        ));
    }

    // TLS秘密鍵の存在確認
    let key_path = Path::new(&config.tls.key_path);
    if !key_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("TLS key not found: {}", config.tls.key_path),
        ));
    }

    Ok(())
}
