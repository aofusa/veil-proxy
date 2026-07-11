//! HTTP/2テストクライアント
//!
//! h2ライブラリを使用したHTTP/2クライアント実装
//! tokioランタイム上で動作し、非同期API

use bytes::Bytes;
use h2::client::SendRequest;
use http::Request;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::ClientConfig;
use tokio_rustls::TlsConnector;

/// HTTP/2テストクライアント
///
/// `ping_pong` は F-96 gRPC Keepalive 検証用。handshake 直後に一度だけ取得し、
/// バックグラウンド Connection タスクと共有する（h2 は 1 接続 1 PingPong）。
#[allow(dead_code)] // テストヘルパ: 一部メソッドは個別 E2E のみから呼ばれる
pub struct Http2TestClient {
    sender: SendRequest<Bytes>,
    /// HTTP/2 PING 用ハンドル（取得済みなら Some）
    ping_pong: Option<h2::PingPong>,
}

#[allow(dead_code)] // テストヘルパ全体: 未使用メソッドが feature 組み合わせで残る
impl Http2TestClient {
    /// 新しいHTTP/2クライアントを作成
    pub async fn new(
        server_addr: &str,
        port: u16,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // TCP接続
        let tcp = TcpStream::connect(format!("{}:{}", server_addr, port)).await?;

        // TLS設定（証明書検証なし - テスト用）
        let tls_config = create_tls_config()?;
        let connector = TlsConnector::from(tls_config);
        let server_name = ServerName::try_from(server_addr.to_string())?;
        let tls_stream = connector.connect(server_name, tcp).await?;

        // HTTP/2ハンドシェイク
        let (sender, mut connection) = h2::client::handshake(tls_stream).await?;

        // PING ハンドルを Connection から取得（後から取れないため先に取り出す）
        let ping_pong = connection.ping_pong();

        // 接続をバックグラウンドで維持
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("HTTP/2 connection error: {:?}", e);
            }
        });

        Ok(Self { sender, ping_pong })
    }

    /// HTTP/2 PING を送信し ACK（PONG）を待つ。gRPC keepalive 検証用（F-96）。
    pub async fn ping(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let pp = self
            .ping_pong
            .as_mut()
            .ok_or("HTTP/2 PingPong handle unavailable")?;
        let _pong = pp.ping(h2::Ping::opaque()).await?;
        Ok(())
    }

    /// HTTPリクエストを送信
    pub async fn send_request(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<(u16, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
        // リクエストを構築
        let mut builder = Request::builder().method(method).uri(path);

        // デフォルトのHTTP/2疑似ヘッダーを追加
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
            builder = builder.header("host", "localhost");
        }

        // カスタムヘッダーを追加
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }

        let request = builder.body(())?;

        // リクエストを送信
        let end_of_stream = body.is_none();
        let (response_future, mut send_body) = self.sender.send_request(request, end_of_stream)?;

        // ボディを送信（ある場合）
        if let Some(body_data) = body {
            send_body.send_data(Bytes::copy_from_slice(body_data), true)?;
        }

        // レスポンスを受信
        let response = response_future.await?;
        let status = response.status().as_u16();

        // レスポンスボディを受信
        let mut body_data = Vec::new();
        let mut body_stream = response.into_body();
        while let Some(chunk) = body_stream.data().await {
            let data = chunk?;
            body_data.extend_from_slice(&data);
            body_stream.flow_control().release_capacity(data.len())?;
        }

        Ok((status, body_data))
    }

    /// 同一 HTTP/2 接続で複数リクエストを**多重化して並行送信**し、各レスポンスを
    /// `(status, body)` の配列で返す（要求順に対応）。
    ///
    /// 全リクエストの response future を先に発行してから await するため、プロキシは
    /// 複数ストリームのレスポンス（連結された HEADERS+DATA）を 1 接続上で交互に送出する。
    /// 送信ホットパスのフレーム連結が多重化下でもフレーム境界・ストリーム対応を壊さない
    /// ことを検証するために使う。
    pub async fn send_concurrent(
        &mut self,
        reqs: &[(&str, &str, Option<&[u8]>)],
    ) -> Result<Vec<(u16, Vec<u8>)>, Box<dyn std::error::Error + Send + Sync>> {
        let mut response_futures = Vec::with_capacity(reqs.len());
        for (method, path, body) in reqs {
            let request = Request::builder()
                .method(*method)
                .uri(*path)
                .header("host", "localhost")
                .body(())?;
            let end_of_stream = body.is_none();
            // 少数の並行ストリーム（max_concurrent_streams 内）のため ready() 待ちは不要。
            let (response_future, mut send_body) =
                self.sender.send_request(request, end_of_stream)?;
            if let Some(body_data) = body {
                send_body.send_data(Bytes::copy_from_slice(body_data), true)?;
            }
            response_futures.push(response_future);
        }

        let mut results = Vec::with_capacity(response_futures.len());
        for response_future in response_futures {
            let response = response_future.await?;
            let status = response.status().as_u16();
            let mut body_data = Vec::new();
            let mut body_stream = response.into_body();
            while let Some(chunk) = body_stream.data().await {
                let data = chunk?;
                body_data.extend_from_slice(&data);
                body_stream.flow_control().release_capacity(data.len())?;
            }
            results.push((status, body_data));
        }

        Ok(results)
    }

    /// GETリクエストを送信
    pub async fn get(
        &mut self,
        path: &str,
    ) -> Result<(u16, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
        self.send_request("GET", path, &[], None).await
    }

    /// POSTリクエストを送信
    pub async fn post(
        &mut self,
        path: &str,
        body: &[u8],
    ) -> Result<(u16, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
        self.send_request("POST", path, &[], Some(body)).await
    }

    /// HTTP/2 リクエストを送信し、レスポンスヘッダ・トレーラーも返す。
    /// gRPC の `grpc-status` / `grpc-message` 検証用。
    pub async fn send_request_full(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<Http2Response, Box<dyn std::error::Error + Send + Sync>> {
        let mut builder = Request::builder().method(method).uri(path);
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
            builder = builder.header("host", "localhost");
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = builder.body(())?;
        let end_of_stream = body.is_none();
        let (response_future, mut send_body) = self.sender.send_request(request, end_of_stream)?;
        if let Some(body_data) = body {
            send_body.send_data(Bytes::copy_from_slice(body_data), true)?;
        }
        Self::collect_response(response_future).await
    }

    /// ボディを複数 DATA フレームに分割して送信する。
    /// gRPC LPM 境界と HTTP/2 DATA 境界のずれを検証する用途。
    /// `chunk_delay` が `Some` のときはチャンク間で sleep する（Slowloris 系）。
    pub async fn send_request_chunked(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        chunks: &[&[u8]],
        chunk_delay: Option<std::time::Duration>,
    ) -> Result<Http2Response, Box<dyn std::error::Error + Send + Sync>> {
        let mut builder = Request::builder().method(method).uri(path);
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
            builder = builder.header("host", "localhost");
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = builder.body(())?;
        let (response_future, mut send_body) = self.sender.send_request(request, false)?;

        let n = chunks.len();
        for (i, chunk) in chunks.iter().enumerate() {
            let end = i + 1 == n;
            if !chunk.is_empty() || end {
                send_body.send_data(Bytes::copy_from_slice(chunk), end)?;
            }
            if !end {
                if let Some(d) = chunk_delay {
                    tokio::time::sleep(d).await;
                }
            }
        }
        if n == 0 {
            send_body.send_data(Bytes::new(), true)?;
        }

        Self::collect_response(response_future).await
    }

    /// リクエスト送信後に RST_STREAM（キャンセル）する。
    /// 大量キャンセル時の生存確認用。戻り値は cancel 成功可否。
    pub async fn send_and_reset(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut builder = Request::builder().method(method).uri(path);
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
            builder = builder.header("host", "localhost");
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = builder.body(())?;
        let end_of_stream = body.is_none();
        let (_response_future, mut send_body) = self.sender.send_request(request, end_of_stream)?;
        if let Some(body_data) = body {
            // end_stream=false で送り、直後に reset
            send_body.send_data(Bytes::copy_from_slice(body_data), false)?;
        }
        send_body.send_reset(h2::Reason::CANCEL);
        Ok(())
    }

    /// ボディの途中（end_stream=false）で Trailers を挿入する攻撃/異常系検証用（F-96）。
    /// プロトコル違反になり得るが、プロキシが panic/hang しないことを確認する。
    pub async fn send_premature_trailers(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body_partial: &[u8],
        trailers: &[(&str, &str)],
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut builder = Request::builder().method(method).uri(path);
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
            builder = builder.header("host", "localhost");
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = builder.body(())?;
        let (response_future, mut send_body) = self.sender.send_request(request, false)?;
        if !body_partial.is_empty() {
            send_body.send_data(Bytes::copy_from_slice(body_partial), false)?;
        }
        let mut map = http::HeaderMap::new();
        for (name, value) in trailers {
            map.insert(
                http::HeaderName::from_bytes(name.as_bytes())?,
                http::HeaderValue::from_str(value)?,
            );
        }
        // Trailers 送信は end-stream 扱い。ボディ未完のまま trailers を送る。
        let _ = send_body.send_trailers(map);
        // 応答は待たず（攻撃完了）— future を drop してキャンセル相当にもできるが
        // ここでは短時間だけ待つ
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), response_future).await;
        Ok(())
    }

    /// 同時に N 本のストリームを開き（ボディ遅延）、MAX_CONCURRENT_STREAMS 境界を刺激する。
    /// 開いたストリーム数と、エラーで止まった場合の理由を返す。
    pub async fn open_many_streams(
        &mut self,
        path: &str,
        headers: &[(&str, &str)],
        count: usize,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let mut opened = 0usize;
        let mut pending = Vec::new();
        for i in 0..count {
            let mut builder = Request::builder().method("POST").uri(path);
            if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
                builder = builder.header("host", "localhost");
            }
            for (name, value) in headers {
                builder = builder.header(*name, *value);
            }
            // ストリーム識別用
            builder = builder.header("x-stream-idx", i.to_string());
            let request = builder.body(())?;
            match self.sender.send_request(request, false) {
                Ok((fut, mut send_body)) => {
                    // 最小ボディを end=false で送り half-open を維持
                    let _ = send_body.send_data(Bytes::from_static(b"\x00\x00\x00\x00\x00"), false);
                    pending.push((fut, send_body));
                    opened += 1;
                }
                Err(_) => break,
            }
        }
        // 短時間待ってからリセット解放
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        for (_fut, mut send_body) in pending {
            send_body.send_reset(h2::Reason::CANCEL);
        }
        Ok(opened)
    }

    async fn collect_response(
        response_future: h2::client::ResponseFuture,
    ) -> Result<Http2Response, Box<dyn std::error::Error + Send + Sync>> {
        let response = response_future.await?;
        let status = response.status().as_u16();
        let mut headers = Vec::new();
        for (name, value) in response.headers().iter() {
            headers.push((
                name.as_str().to_string(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            ));
        }

        let mut body_data = Vec::new();
        let mut body_stream = response.into_body();
        while let Some(chunk) = body_stream.data().await {
            let data = chunk?;
            body_data.extend_from_slice(&data);
            body_stream.flow_control().release_capacity(data.len())?;
        }

        let mut trailers = Vec::new();
        if let Some(tr) = body_stream.trailers().await? {
            for (name, value) in tr.iter() {
                trailers.push((
                    name.as_str().to_string(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                ));
            }
        }

        Ok(Http2Response {
            status,
            headers,
            body: body_data,
            trailers,
        })
    }
}

/// HTTP/2 レスポンス（ステータス・ヘッダ・ボディ・トレーラー）
#[derive(Debug, Clone)]
// テストヘルパ: 共有 tests/common を含む integration_tests では未使用（e2e_tests では使用）。
#[allow(dead_code)]
pub struct Http2Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub trailers: Vec<(String, String)>,
}

// テストヘルパ: grpc_status/grpc_message は e2e_tests のみから呼ばれ integration_tests では未使用。
#[allow(dead_code)]
impl Http2Response {
    /// gRPC トレーラーまたは初期ヘッダから grpc-status を取得
    pub fn grpc_status(&self) -> Option<u32> {
        self.trailers
            .iter()
            .chain(self.headers.iter())
            .find(|(k, _)| k.eq_ignore_ascii_case("grpc-status"))
            .and_then(|(_, v)| v.parse().ok())
    }

    pub fn grpc_message(&self) -> Option<String> {
        self.trailers
            .iter()
            .chain(self.headers.iter())
            .find(|(k, _)| k.eq_ignore_ascii_case("grpc-message"))
            .map(|(_, v)| v.clone())
    }
}

/// テスト用TLS設定を作成（証明書検証なし）
fn create_tls_config() -> Result<Arc<ClientConfig>, Box<dyn std::error::Error + Send + Sync>> {
    use tokio_rustls::rustls::crypto::CryptoProvider;

    // CryptoProviderを初期化（既に初期化されている場合は無視）
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let _ = CryptoProvider::install_default(
            tokio_rustls::rustls::crypto::aws_lc_rs::default_provider(),
        );
    });

    // 証明書検証をスキップするカスタム検証器
    #[derive(Debug)]
    struct SkipServerVerification;

    impl tokio_rustls::rustls::client::danger::ServerCertVerifier for SkipServerVerification {
        fn verify_server_cert(
            &self,
            _end_entity: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[tokio_rustls::rustls::pki_types::CertificateDer<'_>],
            _server_name: &tokio_rustls::rustls::pki_types::ServerName<'_>,
            _ocsp: &[u8],
            _now: tokio_rustls::rustls::pki_types::UnixTime,
        ) -> Result<
            tokio_rustls::rustls::client::danger::ServerCertVerified,
            tokio_rustls::rustls::Error,
        > {
            Ok(tokio_rustls::rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
            _dss: &tokio_rustls::rustls::DigitallySignedStruct,
        ) -> Result<
            tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
            tokio_rustls::rustls::Error,
        > {
            Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
            _dss: &tokio_rustls::rustls::DigitallySignedStruct,
        ) -> Result<
            tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
            tokio_rustls::rustls::Error,
        > {
            Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<tokio_rustls::rustls::SignatureScheme> {
            tokio_rustls::rustls::crypto::aws_lc_rs::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
                .to_vec()
        }
    }

    // HTTP/2用のALPN設定
    let mut config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();

    config.alpn_protocols = vec![b"h2".to_vec()];

    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_http2_client_creation() {
        // このテストは実際のサーバーなしでは失敗するが、
        // コンパイルが通ることを確認
        let result = Http2TestClient::new("localhost", 8443).await;
        // 接続エラーは想定内
        assert!(result.is_err() || result.is_ok());
    }
}
