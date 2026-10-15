//! アクセスログ（F-221。Istio の Telemetry API の accessLogging = Envoy の `access_log`）。
//!
//! Envoy の書式（`%START_TIME%`・`%REQ(X)%` 等のコマンド演算子）の主要なものを扱い、テキストと JSON の
//! どちらでも出す。書き出しは専用スレッド（有界のチャネル。溢れたら捨てる）で、データプレーンは
//! 1 行の文字列を作って渡すだけ（stdout への書き込みで止まらない）。設定が無いときは何もしない。

use super::xds::proto::{self as p};
use bytes::Bytes;
use once_cell::sync::Lazy;
use std::net::SocketAddr;
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Mutex;

pub const T_FILE: &str =
    "type.googleapis.com/envoy.extensions.access_loggers.file.v3.FileAccessLog";
pub const T_STDOUT: &str =
    "type.googleapis.com/envoy.extensions.access_loggers.stream.v3.StdoutAccessLog";
pub const T_STDERR: &str =
    "type.googleapis.com/envoy.extensions.access_loggers.stream.v3.StderrAccessLog";

#[derive(Clone, PartialEq, prost::Message)]
pub struct AccessLogProto {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(message, optional, tag = "4")]
    pub typed_config: Option<p::Any>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct FileAccessLog {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, tag = "2")]
    pub format: String,
    #[prost(message, optional, tag = "3")]
    pub json_format: Option<p::Struct>,
    #[prost(message, optional, tag = "5")]
    pub log_format: Option<SubstitutionFormatString>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct StreamAccessLog {
    #[prost(message, optional, tag = "1")]
    pub log_format: Option<SubstitutionFormatString>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct SubstitutionFormatString {
    #[prost(string, tag = "1")]
    pub text_format: String,
    #[prost(message, optional, tag = "2")]
    pub json_format: Option<p::Struct>,
    #[prost(message, optional, tag = "5")]
    pub text_format_source: Option<p::DataSource>,
}

/// Envoy の既定の書式（`format` を指定しない FileAccessLog）。
const DEFAULT_FORMAT: &str = "[%START_TIME%] \"%REQ(:METHOD)% %REQ(X-ENVOY-ORIGINAL-PATH?:PATH)% %PROTOCOL%\" %RESPONSE_CODE% %RESPONSE_FLAGS% %BYTES_RECEIVED% %BYTES_SENT% %DURATION% %RESP(X-ENVOY-UPSTREAM-SERVICE-TIME)% \"%REQ(X-FORWARDED-FOR)%\" \"%REQ(USER-AGENT)%\" \"%REQ(X-REQUEST-ID)%\" \"%REQ(:AUTHORITY)%\" \"%UPSTREAM_HOST%\"\n";

#[derive(Debug, Clone)]
pub enum Tok {
    Lit(String),
    StartTime,
    Req(String, Option<String>),
    Resp(String),
    Protocol,
    ResponseCode,
    ResponseFlags,
    ResponseCodeDetails,
    ConnTermDetails,
    UpstreamFailureReason,
    BytesReceived,
    BytesSent,
    Duration,
    UpstreamHost,
    UpstreamCluster,
    UpstreamLocalAddress,
    DownstreamLocalAddress,
    DownstreamRemoteAddress,
    DownstreamRemoteAddressNoPort,
    RequestedServerName,
    RouteName,
    Unknown,
}

#[derive(Debug)]
pub enum Format {
    Text(Vec<Tok>),
    Json(Vec<(String, Vec<Tok>)>),
}

#[derive(Debug)]
pub struct AccessLogCfg {
    pub path: String,
    pub format: Format,
}

/// `%...%` の書式を字句に分ける。
pub fn parse_format(s: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut lit = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        lit.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let Some(j) = after.find('%') else {
            lit.push_str(&rest[i..]);
            rest = "";
            break;
        };
        let op = &after[..j];
        if op.is_empty() {
            lit.push('%');
            rest = &after[j + 1..];
            continue;
        }
        if !lit.is_empty() {
            out.push(Tok::Lit(std::mem::take(&mut lit)));
        }
        out.push(op_token(op));
        rest = &after[j + 1..];
    }
    lit.push_str(rest);
    if !lit.is_empty() {
        out.push(Tok::Lit(lit));
    }
    out
}

fn op_token(op: &str) -> Tok {
    // REQ(X?Y):N の :N（長さの上限）は無視する
    let base = match op.rfind("):") {
        Some(i) if op[i + 2..].chars().all(|c| c.is_ascii_digit()) => &op[..i + 1],
        _ => op,
    };
    if let Some(inner) = base.strip_prefix("REQ(").and_then(|x| x.strip_suffix(')')) {
        let (a, b) = match inner.split_once('?') {
            Some((a, b)) => (a.to_ascii_lowercase(), Some(b.to_ascii_lowercase())),
            None => (inner.to_ascii_lowercase(), None),
        };
        return Tok::Req(a, b);
    }
    if let Some(inner) = base.strip_prefix("RESP(").and_then(|x| x.strip_suffix(')')) {
        return Tok::Resp(inner.to_ascii_lowercase());
    }
    match base {
        "START_TIME" => Tok::StartTime,
        "PROTOCOL" => Tok::Protocol,
        "RESPONSE_CODE" => Tok::ResponseCode,
        "RESPONSE_FLAGS" => Tok::ResponseFlags,
        "RESPONSE_CODE_DETAILS" => Tok::ResponseCodeDetails,
        "CONNECTION_TERMINATION_DETAILS" => Tok::ConnTermDetails,
        "UPSTREAM_TRANSPORT_FAILURE_REASON" => Tok::UpstreamFailureReason,
        "BYTES_RECEIVED" => Tok::BytesReceived,
        "BYTES_SENT" => Tok::BytesSent,
        "DURATION" => Tok::Duration,
        "UPSTREAM_HOST" => Tok::UpstreamHost,
        "UPSTREAM_CLUSTER" | "UPSTREAM_CLUSTER_RAW" => Tok::UpstreamCluster,
        "UPSTREAM_LOCAL_ADDRESS" => Tok::UpstreamLocalAddress,
        "DOWNSTREAM_LOCAL_ADDRESS" => Tok::DownstreamLocalAddress,
        "DOWNSTREAM_REMOTE_ADDRESS" | "DOWNSTREAM_PEER_ADDRESS" => Tok::DownstreamRemoteAddress,
        "DOWNSTREAM_REMOTE_ADDRESS_WITHOUT_PORT" => Tok::DownstreamRemoteAddressNoPort,
        "REQUESTED_SERVER_NAME" => Tok::RequestedServerName,
        "ROUTE_NAME" => Tok::RouteName,
        _ => Tok::Unknown,
    }
}

/// xDS の `access_log` を変換する（扱えない種類は無視）。
pub fn translate(list: &[AccessLogProto]) -> Vec<std::sync::Arc<AccessLogCfg>> {
    let mut out = Vec::new();
    for a in list {
        let Some(any) = &a.typed_config else { continue };
        let (path, lf, legacy, legacy_json) = if any.type_url == T_FILE {
            let Some(f) = p::decode_any::<FileAccessLog>(any, T_FILE) else {
                continue;
            };
            (f.path, f.log_format, f.format, f.json_format)
        } else if any.type_url == T_STDOUT || any.type_url == T_STDERR {
            let Some(f) = p::decode_any::<StreamAccessLog>(any, &any.type_url) else {
                continue;
            };
            let path = if any.type_url == T_STDOUT {
                "/dev/stdout"
            } else {
                "/dev/stderr"
            };
            (path.to_string(), f.log_format, String::new(), None)
        } else {
            ftlog::debug!("[mesh] ignoring access log {}", any.type_url);
            continue;
        };
        let format = match (lf, legacy_json) {
            (Some(sfs), _) => match (sfs.json_format, sfs.text_format_source, sfs.text_format) {
                (Some(j), _, _) => json_fields(&j),
                (None, Some(ds), _) => Format::Text(parse_format(&String::from_utf8_lossy(
                    ds.inline().unwrap_or(b""),
                ))),
                (None, None, t) if !t.is_empty() => Format::Text(parse_format(&t)),
                _ => Format::Text(parse_format(DEFAULT_FORMAT)),
            },
            (None, Some(j)) => json_fields(&j),
            (None, None) if !legacy.is_empty() => Format::Text(parse_format(&legacy)),
            (None, None) => Format::Text(parse_format(DEFAULT_FORMAT)),
        };
        out.push(std::sync::Arc::new(AccessLogCfg { path, format }));
    }
    out
}

fn json_fields(s: &p::Struct) -> Format {
    let mut v: Vec<(String, Vec<Tok>)> = s
        .fields
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), parse_format(s))))
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    Format::Json(v)
}

/// 1 件の記録。
pub struct Record<'a> {
    pub start: std::time::SystemTime,
    pub protocol: &'a str,
    pub code: u16,
    pub flags: &'a str,
    pub details: &'a str,
    pub bytes_received: u64,
    pub bytes_sent: u64,
    pub duration_ms: u64,
    pub req: &'a dyn Fn(&str) -> Option<Bytes>,
    pub resp: &'a dyn Fn(&str) -> Option<Bytes>,
    pub upstream_host: Option<SocketAddr>,
    pub cluster: &'a str,
    pub downstream_local: Option<SocketAddr>,
    pub downstream_remote: Option<SocketAddr>,
    pub sni: Option<&'a str>,
    pub route_name: &'a str,
}

fn start_time(t: std::time::SystemTime) -> String {
    let d = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let dt = time::OffsetDateTime::from_unix_timestamp(d.as_secs() as i64)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        d.subsec_millis()
    )
}

fn render(toks: &[Tok], r: &Record<'_>, out: &mut String) {
    use std::fmt::Write;
    let opt = |out: &mut String, v: Option<Bytes>| match v {
        Some(b) if !b.is_empty() => out.push_str(&String::from_utf8_lossy(&b)),
        _ => out.push('-'),
    };
    for t in toks {
        match t {
            Tok::Lit(s) => out.push_str(s),
            Tok::StartTime => out.push_str(&start_time(r.start)),
            Tok::Req(a, b) => {
                let v = (r.req)(a).or_else(|| b.as_ref().and_then(|b| (r.req)(b)));
                opt(out, v)
            }
            Tok::Resp(h) => opt(out, (r.resp)(h)),
            Tok::Protocol => out.push_str(r.protocol),
            Tok::ResponseCode => {
                let _ = write!(out, "{}", r.code);
            }
            Tok::ResponseFlags => out.push_str(r.flags),
            Tok::ResponseCodeDetails => {
                out.push_str(if r.details.is_empty() { "-" } else { r.details })
            }
            Tok::ConnTermDetails | Tok::UpstreamFailureReason | Tok::Unknown => out.push('-'),
            Tok::BytesReceived => {
                let _ = write!(out, "{}", r.bytes_received);
            }
            Tok::BytesSent => {
                let _ = write!(out, "{}", r.bytes_sent);
            }
            Tok::Duration => {
                let _ = write!(out, "{}", r.duration_ms);
            }
            Tok::UpstreamHost => match r.upstream_host {
                Some(a) => {
                    let _ = write!(out, "{}", a);
                }
                None => out.push('-'),
            },
            Tok::UpstreamCluster => {
                out.push_str(if r.cluster.is_empty() { "-" } else { r.cluster })
            }
            Tok::UpstreamLocalAddress => out.push('-'),
            Tok::DownstreamLocalAddress => match r.downstream_local {
                Some(a) => {
                    let _ = write!(out, "{}", a);
                }
                None => out.push('-'),
            },
            Tok::DownstreamRemoteAddress => match r.downstream_remote {
                Some(a) => {
                    let _ = write!(out, "{}", a);
                }
                None => out.push('-'),
            },
            Tok::DownstreamRemoteAddressNoPort => match r.downstream_remote {
                Some(a) => {
                    let _ = write!(out, "{}", a.ip());
                }
                None => out.push('-'),
            },
            Tok::RequestedServerName => out.push_str(r.sni.unwrap_or("-")),
            Tok::RouteName => out.push_str(if r.route_name.is_empty() {
                "-"
            } else {
                r.route_name
            }),
        }
    }
}

/// 1 行を作る。
pub fn format_line(cfg: &AccessLogCfg, r: &Record<'_>) -> String {
    let mut out = String::with_capacity(256);
    match &cfg.format {
        Format::Text(t) => render(t, r, &mut out),
        Format::Json(fields) => {
            let mut m = serde_json::Map::new();
            for (k, toks) in fields {
                let mut v = String::new();
                render(toks, r, &mut v);
                let val = match v.parse::<i64>() {
                    Ok(n) if toks.len() == 1 && !matches!(toks[0], Tok::Lit(_)) => {
                        serde_json::json!(n)
                    }
                    _ if v == "-" => serde_json::Value::Null,
                    _ => serde_json::Value::String(v),
                };
                m.insert(k.clone(), val);
            }
            out = serde_json::Value::Object(m).to_string();
            out.push('\n');
        }
    }
    out
}

struct Writer {
    tx: SyncSender<(String, String)>,
}

static WRITER: Lazy<Mutex<Option<Writer>>> = Lazy::new(|| Mutex::new(None));

fn writer() -> Option<SyncSender<(String, String)>> {
    let mut g = WRITER.lock().ok()?;
    if g.is_none() {
        let (tx, rx) = sync_channel::<(String, String)>(8192);
        let spawned = std::thread::Builder::new()
            .name("veil-accesslog".into())
            .spawn(move || {
                use std::io::Write;
                let mut files: std::collections::HashMap<String, std::fs::File> =
                    Default::default();
                while let Ok((path, line)) = rx.recv() {
                    let r = match path.as_str() {
                        "/dev/stdout" => std::io::stdout().write_all(line.as_bytes()),
                        "/dev/stderr" => std::io::stderr().write_all(line.as_bytes()),
                        _ => {
                            let f = match files.get_mut(&path) {
                                Some(f) => f,
                                None => match std::fs::OpenOptions::new()
                                    .create(true)
                                    .append(true)
                                    .open(&path)
                                {
                                    Ok(f) => files.entry(path.clone()).or_insert(f),
                                    Err(_) => continue,
                                },
                            };
                            f.write_all(line.as_bytes())
                        }
                    };
                    let _ = r;
                }
            });
        if spawned.is_err() {
            return None;
        }
        *g = Some(Writer { tx });
    }
    g.as_ref().map(|w| w.tx.clone())
}

/// 書き出しのスレッドへ渡す（溢れたら捨てる）。
pub fn emit(cfgs: &[std::sync::Arc<AccessLogCfg>], r: &Record<'_>) {
    if cfgs.is_empty() {
        return;
    }
    let Some(tx) = writer() else { return };
    for c in cfgs {
        let _ = tx.try_send((c.path.clone(), format_line(c, r)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_istio_default_line() {
        let fmt = "[%START_TIME%] \"%REQ(:METHOD)% %REQ(X-ENVOY-ORIGINAL-PATH?:PATH)% %PROTOCOL%\" %RESPONSE_CODE% %RESPONSE_FLAGS% %RESPONSE_CODE_DETAILS% %BYTES_RECEIVED% %BYTES_SENT% %DURATION% \"%REQ(USER-AGENT)%\" \"%UPSTREAM_HOST%\" %UPSTREAM_CLUSTER_RAW% %ROUTE_NAME% 100%%\n";
        let cfg = AccessLogCfg {
            path: "/dev/stdout".into(),
            format: Format::Text(parse_format(fmt)),
        };
        let req = |n: &str| match n {
            ":method" => Some(Bytes::from_static(b"GET")),
            ":path" => Some(Bytes::from_static(b"/echo")),
            "user-agent" => Some(Bytes::from_static(b"curl")),
            _ => None,
        };
        let resp = |_: &str| None;
        let r = Record {
            start: std::time::UNIX_EPOCH,
            protocol: "HTTP/1.1",
            code: 200,
            flags: "-",
            details: "via_upstream",
            bytes_received: 0,
            bytes_sent: 12,
            duration_ms: 3,
            req: &req,
            resp: &resp,
            upstream_host: Some("10.0.0.1:8080".parse().unwrap()),
            cluster: "outbound|8080||s",
            downstream_local: None,
            downstream_remote: None,
            sni: None,
            route_name: "default",
        };
        let line = format_line(&cfg, &r);
        assert_eq!(
            line,
            "[1970-01-01T00:00:00.000Z] \"GET /echo HTTP/1.1\" 200 - via_upstream 0 12 3 \"curl\" \"10.0.0.1:8080\" outbound|8080||s default 100%\n"
        );
    }
}
