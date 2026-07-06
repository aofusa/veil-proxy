//! OpenTelemetry 対応（F-10 Phase 1: OTLP/HTTP メトリクスエクスポート）
//!
//! # 設計方針
//!
//! データプレーンは tokio に依存しないため、重い `opentelemetry-sdk`
//! （`rt-tokio` 前提）は使わず、`std::net::TcpStream` によるブロッキング
//! I/O を専用のバックグラウンドスレッドで実行する軽量エクスポータを実装する。
//!
//! OTLP/HTTP は protobuf だけでなく JSON ペイロード（Content-Type:
//! application/json）も受理する。Phase 1 では Prometheus レジストリの
//! カウンタ/ゲージを OTLP メトリクス JSON にブリッジして
//! `<endpoint>/v1/metrics` へ POST する。
//!
//! # スレッドモデル
//!
//! - `std::sync::mpsc::channel` で制御メッセージ（Flush / Shutdown）を受け取る
//! - `batch_interval_secs` ごとに自動 flush
//! - Shutdown 受信でクリーンに終了する

use std::io::Write;
use std::net::TcpStream;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Mutex;
use std::thread::JoinHandle;
use std::time::Duration;

use once_cell::sync::Lazy;

/// OpenTelemetry エクスポート設定（F-10）
#[derive(Clone, Debug)]
pub struct OtelConfig {
    /// 有効化フラグ
    pub enabled: bool,
    /// OTLP/HTTP エンドポイント（例: "http://localhost:4318"）
    pub endpoint: String,
    /// サービス名（resource 属性 service.name）
    pub service_name: String,
    /// バッチ送信間隔（秒）
    pub batch_interval_secs: u64,
}

impl Default for OtelConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: "http://localhost:4318".to_string(),
            service_name: "veil-proxy".to_string(),
            batch_interval_secs: 30,
        }
    }
}

/// バックグラウンドスレッドへの制御メッセージ
enum ExportMsg {
    /// 即時 flush（テスト・SIGHUP 用）
    Flush,
    /// クリーンシャットダウン
    Shutdown,
}

/// エクスポータのハンドル
pub struct OtelExporter {
    sender: Sender<ExportMsg>,
    handle: Option<JoinHandle<()>>,
}

impl OtelExporter {
    /// 即時 flush を要求する
    pub fn flush(&self) {
        let _ = self.sender.send(ExportMsg::Flush);
    }

    /// エクスポータを停止する（スレッドの join まで待つ）
    pub fn shutdown(mut self) {
        let _ = self.sender.send(ExportMsg::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for OtelExporter {
    fn drop(&mut self) {
        if self.handle.is_some() {
            let _ = self.sender.send(ExportMsg::Shutdown);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }
}

/// グローバルエクスポータ（起動時に Some、shutdown 時に None）
static GLOBAL_EXPORTER: Lazy<Mutex<Option<OtelExporter>>> = Lazy::new(|| Mutex::new(None));

/// エクスポータを起動してグローバルに登録する。
///
/// 既に起動済みの場合は何もしない。`enabled = false` の場合も何もしない。
pub fn init_global(config: OtelConfig) {
    if !config.enabled {
        return;
    }
    let mut guard = match GLOBAL_EXPORTER.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if guard.is_some() {
        return;
    }
    match start(config) {
        Ok(exp) => {
            *guard = Some(exp);
            ftlog::info!("OpenTelemetry OTLP/HTTP exporter started");
        }
        Err(e) => {
            ftlog::warn!("Failed to start OpenTelemetry exporter: {}", e);
        }
    }
}

/// グローバルエクスポータの flush を要求する
pub fn flush_global() {
    if let Ok(guard) = GLOBAL_EXPORTER.lock() {
        if let Some(exp) = guard.as_ref() {
            exp.flush();
        }
    }
}

/// グローバルエクスポータを停止する
pub fn shutdown_global() {
    let exp = {
        let mut guard = match GLOBAL_EXPORTER.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        guard.take()
    };
    if let Some(exp) = exp {
        exp.shutdown();
    }
}

/// エクスポータを起動する（バックグラウンドスレッドを spawn）。
pub fn start(config: OtelConfig) -> anyhow::Result<OtelExporter> {
    let (tx, rx): (Sender<ExportMsg>, Receiver<ExportMsg>) = std::sync::mpsc::channel();
    let interval = Duration::from_secs(config.batch_interval_secs.max(1));

    let handle = std::thread::Builder::new()
        .name("otel-exporter".to_string())
        .spawn(move || {
            run_export_loop(config, rx, interval);
        })?;

    Ok(OtelExporter {
        sender: tx,
        handle: Some(handle),
    })
}

/// エクスポートループ本体。
fn run_export_loop(config: OtelConfig, rx: Receiver<ExportMsg>, interval: Duration) {
    loop {
        // interval 待つ間に制御メッセージを受け取る
        match rx.recv_timeout(interval) {
            Ok(ExportMsg::Shutdown) => {
                // 終了前に最後の flush
                let _ = export_once(&config);
                break;
            }
            Ok(ExportMsg::Flush) => {
                let _ = export_once(&config);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // 定期 flush
                let _ = export_once(&config);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                break;
            }
        }
    }
}

/// 1 回分のエクスポートを実行する。
///
/// Prometheus レジストリの値を OTLP/HTTP JSON にブリッジして POST する。
fn export_once(config: &OtelConfig) -> anyhow::Result<()> {
    let body = build_otlp_json(config);
    post_otlp(config, &body)
}

/// Prometheus レジストリから OTLP/HTTP メトリクス JSON を構築する。
///
/// metrics feature が無効の場合は空のメトリクスペイロードを返す。
fn build_otlp_json(config: &OtelConfig) -> String {
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    #[cfg(feature = "metrics")]
    let data_points = {
        // Prometheus レジストリの値をブリッジ
        let families = crate::metrics::gather_metric_families();
        let mut metrics_json = Vec::new();
        for fam in &families {
            let name = json_escape(fam.0.as_str());
            // 各サンプルを Gauge data point として出力（Phase 1 では型を簡略化）
            let mut points = String::new();
            for (labels, value) in &fam.1 {
                if !points.is_empty() {
                    points.push(',');
                }
                let attrs = build_attributes_json(labels);
                points.push_str(&format!(
                    r#"{{"timeUnixNano":"{}","asDouble":{},"attributes":[{}]}}"#,
                    now_ns, value, attrs
                ));
            }
            metrics_json.push(format!(
                r#"{{"name":"{}","gauge":{{"dataPoints":[{}]}}}}"#,
                name, points
            ));
        }
        metrics_json.join(",")
    };

    #[cfg(not(feature = "metrics"))]
    let data_points = {
        let _ = now_ns;
        String::new()
    };

    format!(
        r#"{{"resourceMetrics":[{{"resource":{{"attributes":[{{"key":"service.name","value":{{"stringValue":"{}"}}}}]}},"scopeMetrics":[{{"scope":{{"name":"veil-proxy"}},"metrics":[{}]}}]}}]}}"#,
        json_escape(&config.service_name),
        data_points
    )
}

/// ラベル集合を OTLP 属性 JSON 配列の中身に変換する。
#[cfg_attr(not(feature = "metrics"), allow(dead_code))]
fn build_attributes_json(labels: &[(String, String)]) -> String {
    labels
        .iter()
        .map(|(k, v)| {
            format!(
                r#"{{"key":"{}","value":{{"stringValue":"{}"}}}}"#,
                json_escape(k),
                json_escape(v)
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// JSON 文字列エスケープ（最小限）。
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// OTLP/HTTP エンドポイントへ JSON を POST する（std::net、ブロッキング）。
///
/// `<endpoint>/v1/metrics` に対して最小限の HTTP/1.1 リクエストを送る。
// 理由付き allow: 専用エクスポータスレッドから呼ばれる同期送信（イベントループ外・データプレーン非経由）。
#[allow(clippy::disallowed_methods)]
fn post_otlp(config: &OtelConfig, body: &str) -> anyhow::Result<()> {
    let (host, port, path_base) = parse_endpoint(&config.endpoint)?;
    let path = format!("{}/v1/metrics", path_base.trim_end_matches('/'));
    let addr = format!("{}:{}", host, port);

    let mut stream = TcpStream::connect(&addr)?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;

    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        path = path,
        host = host,
        len = body.len(),
        body = body,
    );
    stream.write_all(request.as_bytes())?;
    stream.flush()?;
    // レスポンスは読み捨て（ベストエフォート）
    Ok(())
}

/// エンドポイント URL を (host, port, path) に分解する。
///
/// 例: "http://localhost:4318" -> ("localhost", 4318, "")
fn parse_endpoint(endpoint: &str) -> anyhow::Result<(String, u16, String)> {
    let without_scheme = endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
        .unwrap_or(endpoint);

    let (host_port, path) = match without_scheme.find('/') {
        Some(i) => (&without_scheme[..i], &without_scheme[i..]),
        None => (without_scheme, ""),
    };

    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => (
            h.to_string(),
            p.parse::<u16>()
                .map_err(|_| anyhow::anyhow!("invalid port in endpoint: {}", endpoint))?,
        ),
        None => (host_port.to_string(), 4318u16),
    };

    if host.is_empty() {
        return Err(anyhow::anyhow!("empty host in endpoint: {}", endpoint));
    }
    Ok((host, port, path.to_string()))
}

#[cfg(test)]
mod tests {
    // 理由付き allow: テストコードは同期 I/O・sleep を使用してよい（データプレーン非経由）。
    #![allow(clippy::disallowed_methods)]
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[test]
    fn parse_endpoint_variants() {
        let (h, p, path) = parse_endpoint("http://localhost:4318").unwrap();
        assert_eq!(h, "localhost");
        assert_eq!(p, 4318);
        assert_eq!(path, "");

        let (h, p, path) = parse_endpoint("https://otel.example.com:443/base").unwrap();
        assert_eq!(h, "otel.example.com");
        assert_eq!(p, 443);
        assert_eq!(path, "/base");

        let (h, p, _) = parse_endpoint("collector").unwrap();
        assert_eq!(h, "collector");
        assert_eq!(p, 4318);
    }

    #[test]
    fn json_escape_handles_specials() {
        assert_eq!(json_escape("a\"b\\c"), "a\\\"b\\\\c");
        assert_eq!(json_escape("line\n"), "line\\n");
    }

    #[test]
    fn build_json_includes_service_name() {
        let cfg = OtelConfig {
            enabled: true,
            service_name: "my-proxy".into(),
            ..Default::default()
        };
        let json = build_otlp_json(&cfg);
        assert!(json.contains("\"service.name\""));
        assert!(json.contains("my-proxy"));
        assert!(json.contains("resourceMetrics"));
    }

    #[test]
    fn exporter_starts_and_stops_cleanly() {
        // モックの OTLP コレクタを起動
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let got_request = Arc::new(AtomicBool::new(false));
        let got_clone = got_request.clone();

        let server = std::thread::spawn(move || {
            listener.set_nonblocking(false).unwrap();
            // 1 件受け付ければ十分
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf);
                if buf.starts_with(b"POST ") {
                    got_clone.store(true, Ordering::SeqCst);
                }
                let _ = sock.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });

        let cfg = OtelConfig {
            enabled: true,
            endpoint: format!("http://{}:{}", addr.ip(), addr.port()),
            service_name: "test".into(),
            batch_interval_secs: 60, // 自動 flush は走らせず、手動 flush で送る
        };
        let exporter = start(cfg).unwrap();
        // 明示的に flush して 1 件送信
        exporter.flush();
        // サーバ側が受け取るのを待つ
        std::thread::sleep(Duration::from_millis(300));
        exporter.shutdown();
        let _ = server.join();

        assert!(
            got_request.load(Ordering::SeqCst),
            "exporter did not POST to collector"
        );
    }
}
