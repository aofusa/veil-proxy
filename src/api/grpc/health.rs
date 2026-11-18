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
    // B-423: アプリが状態を置いた名前か（`Watch` だけで作った項目は、待つ相手が居なくなったら消す）
    known: bool,
    watchers: Vec<mpsc::UnboundedSender<ServingStatus>>,
}

#[derive(Default)]
struct Registry {
    map: HashMap<String, Entry>,
    // 表がこの大きさに達したら、待つ相手の居なくなった項目を掃除する（掃除の後は残りの 2 倍。最小 `SWEEP_MIN`）
    sweep_at: usize,
}

// B-423: 以前は `Watch` の名前（認証無し・最大 4 MiB）ごとに項目と送り口を表へ入れて二度と消さず、名前を変えて
// 呼び続けるだけでメモリが際限なく増えた
const SWEEP_MIN: usize = 64;
// `Watch` が受ける名前の長さの上限（アプリの置く名前はこれより短い）
const MAX_SERVICE_NAME_LEN: usize = 1024;

impl Registry {
    /// 待つ相手の居なくなった送り口を外し、アプリが置いていない名前で待つ相手の居ない項目を消す
    fn sweep(&mut self) {
        self.map.retain(|_, e| {
            e.watchers.retain(|w| !w.is_closed());
            e.known || !e.watchers.is_empty()
        });
        self.sweep_at = (self.map.len() * 2).max(SWEEP_MIN);
    }
}

static REGISTRY: Mutex<Option<Registry>> = Mutex::new(None);

/// Sets the status of a service (sent to clients that are in `Watch`).
pub fn set_serving_status(service: &str, status: ServingStatus) {
    let mut g = REGISTRY.lock().unwrap();
    let reg = g.get_or_insert_with(Registry::default);
    let e = reg.map.entry(service.to_string()).or_insert_with(|| Entry {
        status,
        known: true,
        watchers: Vec::new(),
    });
    e.status = status;
    e.known = true;
    e.watchers.retain(|w| w.unbounded_send(status).is_ok());
}

/// The current status of a service (`None` if it is not registered).
pub fn serving_status(service: &str) -> Option<ServingStatus> {
    REGISTRY
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|r| r.map.get(service))
        .filter(|e| e.known)
        .map(|e| e.status)
}

fn watch(service: &str) -> mpsc::UnboundedReceiver<ServingStatus> {
    let (tx, rx) = mpsc::unbounded();
    let mut g = REGISTRY.lock().unwrap();
    let reg = g.get_or_insert_with(Registry::default);
    match reg.map.get_mut(service) {
        Some(e) => {
            let _ = tx.unbounded_send(e.status);
            e.watchers.retain(|w| !w.is_closed());
            e.watchers.push(tx);
        }
        None => {
            if reg.map.len() >= reg.sweep_at {
                reg.sweep();
            }
            let _ = tx.unbounded_send(ServingStatus::ServiceUnknown);
            reg.map.insert(
                service.to_string(),
                Entry {
                    status: ServingStatus::ServiceUnknown,
                    known: false,
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
                // B-423: 長すぎる名前は登録しない
                if req.get_ref().service.len() > MAX_SERVICE_NAME_LEN {
                    return Err(Status::invalid_argument("service name too long"));
                }
                let rx = watch(&req.get_ref().service);
                Ok(Response::new(
                    rx.map(|s| Ok(HealthCheckResponse { status: s as i32 })),
                ))
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // B-423: `Watch` だけで作った項目は、待つ相手が居なくなれば掃除で消え、表は生きている相手の数で抑えられる。
    // アプリが置いた名前は残り、後から置いた状態は待っている相手へ届く
    #[test]
    fn watch_entries_for_unknown_names_are_swept() {
        set_serving_status("b423.known", ServingStatus::Serving);
        let size = || REGISTRY.lock().unwrap().as_ref().map_or(0, |r| r.map.len());
        for i in 0..10_000 {
            drop(watch(&format!("b423.gone.{i}")));
        }
        assert!(size() < 1_000, "{}", size());
        assert_eq!(serving_status("b423.known"), Some(ServingStatus::Serving));
        assert_eq!(serving_status("b423.gone.1"), None);

        let mut rx = watch("b423.later");
        assert_eq!(rx.try_recv().unwrap(), ServingStatus::ServiceUnknown);
        for i in 0..1_000 {
            drop(watch(&format!("b423.gone2.{i}")));
        }
        set_serving_status("b423.later", ServingStatus::NotServing);
        assert_eq!(rx.try_recv().unwrap(), ServingStatus::NotServing);
    }
}
