// F-192
//! Native L4 (TCP/UDP) handlers (the `l4-handler` feature).
//!
//! With `handler = "<name>"` in `[[l4]]`, that listener does not forward to an upstream; it handles
//! connections (TCP) or datagrams (UDP) with the registered Rust function. A listener without `handler`
//! keeps the existing splice path (code path and performance are unchanged).

use super::app::App;
use crate::runtime::io::AsyncWriteRentExt as _;
use crate::runtime::tcp::TcpStream;
use crate::runtime::time::timeout;
use bytes::Bytes;
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

/// Errors of an L4 handler.
#[derive(Debug)]
pub enum L4Error {
    /// An I/O error.
    Io(std::io::Error),
    /// Idle timeout (`[[l4]] idle_timeout_secs`).
    Timeout,
    /// Any other error (message only).
    Other(String),
}

impl std::fmt::Display for L4Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            L4Error::Io(e) => write!(f, "io error: {}", e),
            L4Error::Timeout => f.write_str("idle timeout"),
            L4Error::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for L4Error {}

impl From<std::io::Error> for L4Error {
    fn from(e: std::io::Error) -> L4Error {
        L4Error::Io(e)
    }
}

impl From<String> for L4Error {
    fn from(e: String) -> L4Error {
        L4Error::Other(e)
    }
}

/// A TCP connection passed to the handler. Reads and writes are asynchronous I/O on the event loop (they do not block).
pub struct L4Connection {
    stream: TcpStream,
    peer: SocketAddr,
    listener: Arc<str>,
    idle_timeout: Duration,
    /// A reusable read buffer (allocated once per connection). `rbuf[rpos..]` is the unconsumed data.
    rbuf: Vec<u8>,
    rpos: usize,
}

/// Maximum number of bytes read in one receive (the capacity of the read buffer).
const L4_READ_CHUNK: usize = 16 * 1024;

impl L4Connection {
    pub(crate) fn new(
        stream: TcpStream,
        peer: SocketAddr,
        listener: Arc<str>,
        idle_timeout: Duration,
    ) -> L4Connection {
        L4Connection {
            stream,
            peer,
            listener,
            idle_timeout,
            rbuf: Vec::new(),
            rpos: 0,
        }
    }

    /// The peer's address.
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    /// The name of the `[[l4]]` listener.
    pub fn listener_name(&self) -> &str {
        &self.listener
    }

    /// Reads at most `buf.len()` bytes. 0 means the peer closed its sending side (EOF).
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize, L4Error> {
        let avail = self.fill().await?;
        let n = avail.min(buf.len());
        buf[..n].copy_from_slice(&self.rbuf[self.rpos..self.rpos + n]);
        self.rpos += n;
        Ok(n)
    }

    /// Reads at most `max` bytes and returns them as `Bytes` (empty means EOF).
    pub async fn read_bytes(&mut self, max: usize) -> Result<Bytes, L4Error> {
        let avail = self.fill().await?;
        let n = avail.min(max);
        let out = Bytes::copy_from_slice(&self.rbuf[self.rpos..self.rpos + n]);
        self.rpos += n;
        Ok(out)
    }

    /// Receives once if there is no unconsumed data. Returns the number of unconsumed bytes (0 means EOF).
    async fn fill(&mut self) -> Result<usize, L4Error> {
        if self.rpos < self.rbuf.len() {
            return Ok(self.rbuf.len() - self.rpos);
        }
        let mut b = std::mem::take(&mut self.rbuf);
        b.clear();
        b.reserve(L4_READ_CHUNK);
        self.rpos = 0;
        let (res, b) = match timeout(self.idle_timeout, self.stream.read(b)).await {
            Ok(r) => r,
            Err(_) => return Err(L4Error::Timeout),
        };
        self.rbuf = b;
        let n = res?;
        self.rbuf.truncate(n);
        Ok(n)
    }

    /// Writes everything (returns the number of bytes written).
    pub async fn write(&mut self, data: &[u8]) -> Result<usize, L4Error> {
        self.write_all(data).await?;
        Ok(data.len())
    }

    /// Writes everything.
    pub async fn write_all(&mut self, data: &[u8]) -> Result<(), L4Error> {
        self.write_bytes(Bytes::copy_from_slice(data)).await
    }

    /// Writes all of a `Bytes` without copying it.
    pub async fn write_bytes(&mut self, data: Bytes) -> Result<(), L4Error> {
        if data.is_empty() {
            return Ok(());
        }
        match timeout(self.idle_timeout, self.stream.write_all(data)).await {
            Ok((r, _)) => {
                r?;
                Ok(())
            }
            Err(_) => Err(L4Error::Timeout),
        }
    }

    /// Closes the sending side (signals EOF to the peer).
    pub fn shutdown_write(&mut self) -> Result<(), L4Error> {
        self.stream
            .shutdown(std::net::Shutdown::Write)
            .map_err(L4Error::Io)
    }
}

/// A datagram passed to a UDP handler. A returned `Some(bytes)` is sent back to the sender.
#[derive(Debug, Clone)]
pub struct L4Datagram {
    /// The contents of the received datagram.
    pub data: Bytes,
    /// The sender's address.
    pub peer: SocketAddr,
    /// The name of the `[[l4]]` listener that received it.
    pub listener: Arc<str>,
}

/// A TCP handler function.
pub type L4TcpFn = Arc<
    dyn for<'a> Fn(&'a mut L4Connection) -> Pin<Box<dyn Future<Output = Result<(), L4Error>> + 'a>>
        + Send
        + Sync,
>;

/// A UDP handler function.
pub type L4UdpFn =
    Arc<dyn Fn(L4Datagram) -> Pin<Box<dyn Future<Output = Option<Bytes>>>> + Send + Sync>;

/// Static registration by the macro (`#[veil::l4_handler(protocol = "tcp")]`).
pub struct L4HandlerDef {
    /// The name of the handler (referenced by `handler` in `[[l4]]`).
    pub name: &'static str,
    /// The handler body.
    pub handler:
        for<'a> fn(&'a mut L4Connection) -> Pin<Box<dyn Future<Output = Result<(), L4Error>> + 'a>>,
}
inventory::collect!(L4HandlerDef);

/// Static registration by the macro (`#[veil::l4_handler(protocol = "udp")]`).
pub struct L4UdpHandlerDef {
    /// The name of the handler (referenced by `handler` in `[[l4]]`).
    pub name: &'static str,
    /// The handler body.
    pub handler: fn(L4Datagram) -> Pin<Box<dyn Future<Output = Option<Bytes>>>>,
}
inventory::collect!(L4UdpHandlerDef);

#[derive(Clone)]
pub(crate) enum L4Handler {
    Tcp(L4TcpFn),
    Udp(L4UdpFn),
}

static HANDLERS: once_cell::sync::Lazy<RwLock<HashMap<String, L4Handler>>> =
    once_cell::sync::Lazy::new(|| RwLock::new(HashMap::new()));

/// The `[[l4]]` tables injected by `App::l4_tcp` / `l4_udp`.
pub(crate) static INJECTED: Mutex<Vec<toml::Value>> = Mutex::new(Vec::new());

fn register(name: &str, h: L4Handler) -> Result<(), String> {
    let mut m = HANDLERS.write().unwrap();
    if m.contains_key(name) {
        return Err(format!("L4 handler '{}' is already registered", name));
    }
    m.insert(name.to_string(), h);
    Ok(())
}

/// Registers the L4 handlers defined by macros (only once, however often it is called).
pub(crate) fn register_macro_handlers() -> Result<(), String> {
    static DONE: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    DONE.get_or_init(|| {
        for d in inventory::iter::<L4HandlerDef> {
            let f = d.handler;
            register(d.name, L4Handler::Tcp(Arc::new(move |c| f(c))))?;
        }
        for d in inventory::iter::<L4UdpHandlerDef> {
            let f = d.handler;
            register(d.name, L4Handler::Udp(Arc::new(move |g| f(g))))?;
        }
        Ok(())
    })
    .clone()
}

/// Looks up a handler by name (resolves `[[l4]] handler`).
pub(crate) fn lookup(name: &str) -> Option<L4Handler> {
    HANDLERS.read().unwrap().get(name).cloned()
}

/// Whether the `[[l4]] handler` name is registered as TCP or UDP (for configuration validation).
pub(crate) fn protocol_of(name: &str) -> Option<&'static str> {
    lookup(name).map(|h| match h {
        L4Handler::Tcp(_) => "tcp",
        L4Handler::Udp(_) => "udp",
    })
}

fn l4_table(name: &str, listen: &str, protocol: &str) -> toml::Value {
    let mut t = toml::Table::new();
    t.insert("name".into(), name.into());
    t.insert("listen".into(), listen.into());
    t.insert("protocol".into(), protocol.into());
    t.insert("handler".into(), name.into());
    t.insert("upstreams".into(), toml::Value::Array(Vec::new()));
    toml::Value::Table(t)
}

impl App {
    /// Opens the TCP listener `listen` and handles connections with `f` (injects an `[[l4]]` entry).
    pub fn l4_tcp<F>(self, name: &str, listen: &str, f: F) -> App
    where
        F: for<'a> Fn(
                &'a mut L4Connection,
            ) -> Pin<Box<dyn Future<Output = Result<(), L4Error>> + 'a>>
            + Send
            + Sync
            + 'static,
    {
        let name_s = name.to_string();
        let table = l4_table(name, listen, "tcp");
        self.on_setup(move || {
            register(&name_s, L4Handler::Tcp(Arc::new(f)))?;
            INJECTED.lock().unwrap().push(table);
            Ok(())
        })
    }

    /// Opens the UDP listener `listen` and handles datagrams with `f` (returning `Some` replies to the sender).
    pub fn l4_udp<F, Fut>(self, name: &str, listen: &str, f: F) -> App
    where
        F: Fn(L4Datagram) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Option<Bytes>> + 'static,
    {
        let name_s = name.to_string();
        let table = l4_table(name, listen, "udp");
        let f = Arc::new(f);
        self.on_setup(move || {
            let h: L4UdpFn = Arc::new(move |g| {
                let fut = f(g);
                Box::pin(fut)
            });
            register(&name_s, L4Handler::Udp(h))?;
            INJECTED.lock().unwrap().push(table);
            Ok(())
        })
    }
}

// ===================== ブロッキングな L4 ハンドラ（C・Python 用、F-210） =====================

/// Output of a blocking L4 handler (data written inside the callback, and whether to close).
#[derive(Default)]
pub struct L4Out {
    writes: Vec<Bytes>,
    close: bool,
}

impl L4Out {
    /// Adds data to send (sent together after the callback returns).
    pub fn write(&mut self, data: Bytes) {
        if !data.is_empty() {
            self.writes.push(data);
        }
    }

    /// Closes the connection after the data has been sent.
    pub fn close(&mut self) {
        self.close = true;
    }
}

/// The state of one TCP connection (created per connection; callbacks are called one at a time, in order, per connection).
pub trait L4Session: Send {
    /// Received data.
    fn on_data(&mut self, data: &[u8], out: &mut L4Out);
    /// The connection closed (the peer closed, `close` was called, or an error occurred).
    fn on_close(&mut self) {}
}

/// Calls a blocking callback on the handler pool (a panic closes the connection).
async fn blocking<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> Option<R> {
    crate::runtime::offload::offload_handler(f).await
}

async fn flush(conn: &mut L4Connection, out: &mut L4Out) -> Result<(), L4Error> {
    for b in out.writes.drain(..) {
        conn.write_bytes(b).await?;
    }
    Ok(())
}

impl App {
    // F-210: C・Python 用。
    /// A TCP handler with blocking callbacks. `connect` is called on the handler pool for each connection and
    /// returns an [`L4Session`] (`None` closes the connection). Reads and writes use the worker's asynchronous I/O,
    /// and the callback is called on the pool once per receive (callbacks of the same connection never run concurrently).
    pub fn l4_tcp_blocking<F>(self, name: &str, listen: &str, connect: F) -> App
    where
        F: Fn(SocketAddr, &mut L4Out) -> Option<Box<dyn L4Session>> + Send + Sync + 'static,
    {
        let connect = Arc::new(connect);
        self.l4_tcp(name, listen, move |conn: &mut L4Connection| {
            let connect = connect.clone();
            Box::pin(async move {
                let peer = conn.peer_addr();
                let Some((session, mut out)) = blocking(move || {
                    let mut out = L4Out::default();
                    (connect(peer, &mut out), out)
                })
                .await
                else {
                    return Ok(());
                };
                flush(conn, &mut out).await?;
                let Some(session) = session else {
                    return Ok(());
                };
                let session = Arc::new(Mutex::new(session));
                let mut result = Ok(());
                if !out.close {
                    loop {
                        let data = match conn.read_bytes(L4_READ_CHUNK).await {
                            Ok(d) if d.is_empty() => break,
                            Ok(d) => d,
                            Err(e) => {
                                result = Err(e);
                                break;
                            }
                        };
                        let s = session.clone();
                        let Some(mut out) = blocking(move || {
                            let mut out = L4Out::default();
                            s.lock().unwrap().on_data(&data, &mut out);
                            out
                        })
                        .await
                        else {
                            break;
                        };
                        if let Err(e) = flush(conn, &mut out).await {
                            result = Err(e);
                            break;
                        }
                        if out.close {
                            break;
                        }
                    }
                }
                let s = session.clone();
                let _ = blocking(move || s.lock().unwrap().on_close()).await;
                result
            })
        })
    }

    // F-210: C・Python 用。
    /// A UDP handler with blocking callbacks. `f` is called on the handler pool for each datagram, and returning
    /// `Some` replies to the sender.
    pub fn l4_udp_blocking<F>(self, name: &str, listen: &str, f: F) -> App
    where
        F: Fn(Bytes, SocketAddr) -> Option<Bytes> + Send + Sync + 'static,
    {
        let f = Arc::new(f);
        self.l4_udp(name, listen, move |g: L4Datagram| {
            let f = f.clone();
            async move { blocking(move || f(g.data, g.peer)).await.flatten() }
        })
    }
}

/// Configuration hook: injects the `[[l4]]` entries of `App::l4_*` (not injected if TOML already has an `[[l4]]` of the same name).
pub(crate) fn inject_l4(root: &mut toml::Value) -> Result<(), String> {
    let injected = INJECTED.lock().unwrap().clone();
    if injected.is_empty() {
        return Ok(());
    }
    let table = root
        .as_table_mut()
        .ok_or_else(|| "config root must be a table".to_string())?;
    let arr = table
        .entry("l4")
        .or_insert_with(|| toml::Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| "'l4' must be an array of tables".to_string())?;
    for t in injected {
        let name = t.get("name").and_then(|n| n.as_str()).unwrap_or("");
        if !arr
            .iter()
            .any(|e| e.get("name").and_then(|n| n.as_str()) == Some(name))
        {
            arr.push(t);
        }
    }
    Ok(())
}

/// Handles a connection with a TCP handler (called from the accept loop of `l4::server`).
pub(crate) async fn serve_tcp(
    handler: L4TcpFn,
    stream: TcpStream,
    peer: SocketAddr,
    listener: Arc<str>,
    idle_timeout: Duration,
) {
    let mut conn = L4Connection::new(stream, peer, listener.clone(), idle_timeout);
    if let Err(e) = handler(&mut conn).await {
        ftlog::debug!("[L4:{}] handler error from {}: {}", listener, peer, e);
    }
}

/// Serves a listener with a UDP handler (called once per datagram; the reply goes out from the same socket).
pub(crate) async fn serve_udp(handler: L4UdpFn, listen_addr: SocketAddr, listener: Arc<str>) {
    use crate::config::SHUTDOWN_FLAG;
    use std::sync::atomic::Ordering;
    let socket = match crate::runtime::udp::UdpSocket::bind(listen_addr) {
        Ok(s) => std::rc::Rc::new(s),
        Err(e) => {
            ftlog::error!("[L4:{}] UDP bind error on {}: {}", listener, listen_addr, e);
            return;
        }
    };
    ftlog::info!("[L4:{}] UDP handler listening on {}", listener, listen_addr);
    let mut buf = vec![0u8; 65536];
    let pool = crate::runtime::TaskPool::new();
    loop {
        if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
            break;
        }
        let (n, peer) = match timeout(Duration::from_secs(1), socket.recv_from(&mut buf)).await {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                ftlog::warn!("[L4:{}] UDP recvfrom error: {}", listener, e);
                continue;
            }
            Err(_) => continue,
        };
        let dgram = L4Datagram {
            data: Bytes::copy_from_slice(&buf[..n]),
            peer,
            listener: listener.clone(),
        };
        let fut = handler(dgram);
        let socket = socket.clone();
        crate::system::spawn_pooled_with_panic_catch(&pool, async move {
            if let Some(reply) = fut.await {
                let _ = socket.send_to(&reply, peer).await;
            }
        });
    }
}
