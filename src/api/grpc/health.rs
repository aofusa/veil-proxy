// F-197
//! The standard health-check service `grpc.health.v1.Health`, registered with `App::grpc_health()`.
//!
//! - `Check`: returns the status of a registered service name (the empty string means the whole server).
//!   An unregistered name gives `NOT_FOUND`.
//! - `Watch`: sends the current status, then again every time `set_serving_status` changes it.
//!
//! The status is shared by the process (the same whichever worker receives the call). The whole server (`""`) is `SERVING` once registered.

use super::{wire, Codec, Request, Response, Status};
use crate::api::app::App;
use bytes::{Bytes, BytesMut};
use futures::channel::mpsc;
use std::collections::HashMap;
use std::sync::Mutex;

/// `grpc.health.v1.HealthCheckRequest`
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HealthCheckRequest {
    /// 1: `string service`
    pub service: String,
}

/// `grpc.health.v1.HealthCheckResponse`
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HealthCheckResponse {
    /// Field 1: `ServingStatus status` (the numeric value of the enum).
    pub status: i32,
}

/// `grpc.health.v1.HealthCheckResponse.ServingStatus`
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(i32)]
pub enum ServingStatus {
    /// The status is unknown.
    Unknown = 0,
    /// The service can handle requests.
    Serving = 1,
    /// The service cannot handle requests.
    NotServing = 2,
    /// Used only by `Watch` (for an unregistered service).
    ServiceUnknown = 3,
}

// F-208: veil は protobuf の実装に依存しない。
/// Hand-written encoding of the two health-check messages.
#[derive(Clone, Copy, Debug, Default)]
pub struct HealthCodec;

impl Codec<HealthCheckRequest> for HealthCodec {
    fn encode(&self, m: &HealthCheckRequest, out: &mut BytesMut) -> Result<(), Status> {
        wire::put_bytes_field(out, 1, m.service.as_bytes());
        Ok(())
    }
    fn decode(&self, buf: Bytes) -> Result<HealthCheckRequest, Status> {
        let mut r = wire::Reader::new(&buf);
        let mut m = HealthCheckRequest::default();
        while let Some((field, w)) = r.next_key()? {
            match (field, w) {
                (1, wire::LEN) => m.service = r.read_string()?,
                _ => r.skip(w)?,
            }
        }
        Ok(m)
    }
    fn encoded_len(&self, m: &HealthCheckRequest) -> usize {
        wire::bytes_field_len(1, m.service.len())
    }
}

impl Codec<HealthCheckResponse> for HealthCodec {
    fn encode(&self, m: &HealthCheckResponse, out: &mut BytesMut) -> Result<(), Status> {
        wire::put_int32_field(out, 1, m.status);
        Ok(())
    }
    fn decode(&self, buf: Bytes) -> Result<HealthCheckResponse, Status> {
        let mut r = wire::Reader::new(&buf);
        let mut m = HealthCheckResponse::default();
        while let Some((field, w)) = r.next_key()? {
            match (field, w) {
                (1, wire::VARINT) => m.status = r.read_varint()? as i32,
                _ => r.skip(w)?,
            }
        }
        Ok(m)
    }
    fn encoded_len(&self, _m: &HealthCheckResponse) -> usize {
        11
    }
}

struct Entry {
    status: ServingStatus,
    watchers: Vec<mpsc::UnboundedSender<ServingStatus>>,
}

static REGISTRY: Mutex<Option<HashMap<String, Entry>>> = Mutex::new(None);

/// Sets the status of a service (sent to clients that are in `Watch`).
pub fn set_serving_status(service: &str, status: ServingStatus) {
    let mut g = REGISTRY.lock().unwrap();
    let map = g.get_or_insert_with(HashMap::new);
    let e = map.entry(service.to_string()).or_insert_with(|| Entry {
        status,
        watchers: Vec::new(),
    });
    e.status = status;
    e.watchers.retain(|w| w.unbounded_send(status).is_ok());
}

/// The current status of a service (`None` if it is not registered).
pub fn serving_status(service: &str) -> Option<ServingStatus> {
    REGISTRY
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|m| m.get(service))
        .map(|e| e.status)
}

fn watch(service: &str) -> mpsc::UnboundedReceiver<ServingStatus> {
    let (tx, rx) = mpsc::unbounded();
    let mut g = REGISTRY.lock().unwrap();
    let map = g.get_or_insert_with(HashMap::new);
    match map.get_mut(service) {
        Some(e) => {
            let _ = tx.unbounded_send(e.status);
            e.watchers.push(tx);
        }
        None => {
            let _ = tx.unbounded_send(ServingStatus::ServiceUnknown);
            map.insert(
                service.to_string(),
                Entry {
                    status: ServingStatus::ServiceUnknown,
                    watchers: vec![tx],
                },
            );
        }
    }
    rx
}

impl App {
    /// Registers `grpc.health.v1.Health` (`Check` and `Watch`). The whole server (`""`) is `SERVING`.
    pub fn grpc_health(self) -> App {
        if serving_status("").is_none() {
            set_serving_status("", ServingStatus::Serving);
        }
        self.grpc_unary(
            "/grpc.health.v1.Health/Check",
            HealthCodec,
            |req: Request<HealthCheckRequest>| async move {
                match serving_status(&req.get_ref().service) {
                    Some(s) if s != ServingStatus::ServiceUnknown => {
                        Ok(Response::new(HealthCheckResponse { status: s as i32 }))
                    }
                    _ => Err(Status::not_found("unknown service")),
                }
            },
        )
        .grpc_server_streaming(
            "/grpc.health.v1.Health/Watch",
            HealthCodec,
            |req: Request<HealthCheckRequest>| async move {
                use futures::StreamExt;
                let rx = watch(&req.get_ref().service);
                Ok(Response::new(
                    rx.map(|s| Ok(HealthCheckResponse { status: s as i32 })),
                ))
            },
        )
    }
}
