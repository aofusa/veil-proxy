//! 標準のヘルスチェックサービス `grpc.health.v1.Health`（F-197）。`App::grpc_health()` で登録する。
//!
//! - `Check`: 登録済みのサービス名（空文字 = サーバ全体）の状態を返す。未登録は `NOT_FOUND`。
//! - `Watch`: 現在の状態を送り、`set_serving_status` で変わるたびに送る。
//!
//! 状態はプロセス共有（どのワーカーで受けても同じ）。サーバ全体（`""`）は登録時に `SERVING`。

use super::{Request, Response, Status};
use crate::api::app::App;
use futures::channel::mpsc;
use std::collections::HashMap;
use std::sync::Mutex;

/// `grpc.health.v1.HealthCheckRequest`
#[derive(Clone, PartialEq, prost::Message)]
pub struct HealthCheckRequest {
    #[prost(string, tag = "1")]
    pub service: String,
}

/// `grpc.health.v1.HealthCheckResponse`
#[derive(Clone, PartialEq, prost::Message)]
pub struct HealthCheckResponse {
    #[prost(enumeration = "ServingStatus", tag = "1")]
    pub status: i32,
}

/// `grpc.health.v1.HealthCheckResponse.ServingStatus`
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, prost::Enumeration)]
#[repr(i32)]
pub enum ServingStatus {
    Unknown = 0,
    Serving = 1,
    NotServing = 2,
    /// `Watch` だけが使う（未登録のサービス）
    ServiceUnknown = 3,
}

struct Entry {
    status: ServingStatus,
    watchers: Vec<mpsc::UnboundedSender<ServingStatus>>,
}

static REGISTRY: Mutex<Option<HashMap<String, Entry>>> = Mutex::new(None);

/// サービスの状態を設定する（`Watch` 中のクライアントへ送る）。
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

/// サービスの現在の状態（未登録なら `None`）。
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
    /// `grpc.health.v1.Health`（Check・Watch）を登録する。サーバ全体（`""`）は `SERVING`。
    pub fn grpc_health(self) -> App {
        if serving_status("").is_none() {
            set_serving_status("", ServingStatus::Serving);
        }
        self.grpc_unary(
            "/grpc.health.v1.Health/Check",
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
