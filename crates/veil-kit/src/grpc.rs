// F-267: gRPC の補助。prost の codec・接続の便利関数・複数の宛先のラウンドロビン・unary の再試行。
//! gRPC helpers on top of [`veil::api::grpc`] (server and client) and prost.
//!
//! Generated code (`veil-build`) already has a `ProstCodec` per service module; [`ProstCodec`] here is the same
//! codec for hand-written registrations and calls.
//!
//! ```no_run
//! # async fn f() -> Result<(), veil::api::grpc::Status> {
//! use veil_kit::grpc::{Balanced, ProstCodec};
//! use veil::api::grpc::Request;
//! let lb = Balanced::connect(&["http://10.0.0.1:50051", "http://10.0.0.2:50051"])?;
//! let reply: veil::api::grpc::Response<prost_types_like::Empty> =
//!     lb.unary("/pkg.Svc/Ping", ProstCodec, Request::new(prost_types_like::Empty {})).await?;
//! # Ok(()) }
//! # mod prost_types_like { #[derive(Clone, PartialEq, prost::Message)] pub struct Empty {} }
//! ```

pub use veil::api::grpc::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Encodes and decodes any prost message.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProstCodec;

impl<T: prost::Message + Default + 'static> Codec<T> for ProstCodec {
    fn encode(&self, message: &T, out: &mut BytesMut) -> Result<(), Status> {
        prost::Message::encode(message, out).map_err(|e| Status::internal(e.to_string()))
    }
    fn decode(&self, buf: Bytes) -> Result<T, Status> {
        <T as prost::Message>::decode(buf)
            .map_err(|e| Status::internal(format!("failed to decode message: {}", e)))
    }
    fn encoded_len(&self, message: &T) -> usize {
        prost::Message::encoded_len(message)
    }
}

/// A channel to `url` (`http://` = h2c, `https://` = h2 over TLS) with a default deadline.
pub fn connect(url: &str, timeout: Duration) -> Result<Channel, Status> {
    Ok(Channel::connect(url)?.timeout(timeout))
}

/// Round robin over several channels. Unary calls that fail with `UNAVAILABLE` are retried on the next server
/// (up to once per server).
#[derive(Clone, Debug)]
pub struct Balanced {
    channels: Arc<Vec<Channel>>,
    next: Arc<AtomicUsize>,
}

impl Balanced {
    /// Channels to `urls` (at least one).
    pub fn connect(urls: &[&str]) -> Result<Balanced, Status> {
        let chans = urls
            .iter()
            .map(|u| Channel::connect(u))
            .collect::<Result<Vec<_>, _>>()?;
        Balanced::new(chans)
    }

    /// From existing channels (at least one).
    pub fn new(channels: Vec<Channel>) -> Result<Balanced, Status> {
        if channels.is_empty() {
            return Err(Status::invalid_argument(
                "Balanced needs at least one channel",
            ));
        }
        Ok(Balanced {
            channels: Arc::new(channels),
            next: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// The next channel in turn (for generated clients: `XClient::new(lb.pick())`).
    pub fn pick(&self) -> Channel {
        let i = self.next.fetch_add(1, Ordering::Relaxed) % self.channels.len();
        self.channels[i].clone()
    }

    /// A unary call on the next server, moving on to the following server on `UNAVAILABLE`.
    pub async fn unary<C, Req, Resp>(
        &self,
        path: &str,
        codec: C,
        req: Request<Req>,
    ) -> Result<Response<Resp>, Status>
    where
        C: Codec<Req> + Codec<Resp>,
        Req: Clone,
        Resp: 'static,
    {
        let n = self.channels.len();
        let start = self.next.fetch_add(1, Ordering::Relaxed);
        let meta: Vec<(Bytes, Bytes)> = req.metadata().to_vec();
        let deadline = req.deadline();
        let msg = req.into_inner();
        let mut last = Status::unavailable("no server");
        for k in 0..n {
            let ch = &self.channels[(start + k) % n];
            let mut r = Request::new(msg.clone());
            for (name, value) in &meta {
                r = r.with_metadata(name.clone(), value.clone());
            }
            if let Some(d) = deadline {
                r = r.with_timeout(d.saturating_duration_since(std::time::Instant::now()));
            }
            match ch.unary(path, codec.clone(), r).await {
                Err(s) if s.code() == Code::Unavailable => last = s,
                other => return other,
            }
        }
        Err(last)
    }
}
