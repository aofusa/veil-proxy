//! L4（TCP/UDP）ネイティブハンドラ（F-192、`l4-handler` feature）。
//!
//! `[[l4]]` に `handler = "<name>"` を書くと、そのリスナーは上流へ転送せず登録した Rust 関数で
//! 接続（TCP）・データグラム（UDP）を処理する。`handler` の無いリスナーは従来の splice 経路のまま
//! （コード経路・性能とも不変）。

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

/// L4 ハンドラのエラー。
#[derive(Debug)]
pub enum L4Error {
    Io(std::io::Error),
    /// アイドルタイムアウト（`[[l4]] idle_timeout_secs`）
    Timeout,
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

/// ハンドラに渡す TCP 接続。読み書きはイベントループ上の非同期 I/O（ブロックしない）。
pub struct L4Connection {
    stream: TcpStream,
    peer: SocketAddr,
    listener: Arc<str>,
    idle_timeout: Duration,
    /// 読み取り用の再利用バッファ（接続ごとに 1 回だけ確保）。`rbuf[rpos..]` が未消費のデータ。
    rbuf: Vec<u8>,
    rpos: usize,
}

/// 1 回の受信で読む最大バイト数（読み取りバッファの容量）。
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

    /// 相手のアドレス。
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    /// `[[l4]]` のリスナー名。
    pub fn listener_name(&self) -> &str {
        &self.listener
    }

    /// 最大 `buf.len()` バイト読む。0 は相手が送信を閉じた（EOF）。
    pub async fn read(&mut self, buf: &mut [u8]) -> Result<usize, L4Error> {
        let avail = self.fill().await?;
        let n = avail.min(buf.len());
        buf[..n].copy_from_slice(&self.rbuf[self.rpos..self.rpos + n]);
        self.rpos += n;
        Ok(n)
    }

    /// 最大 `max` バイト読んで `Bytes` で返す（空なら EOF）。
    pub async fn read_bytes(&mut self, max: usize) -> Result<Bytes, L4Error> {
        let avail = self.fill().await?;
        let n = avail.min(max);
        let out = Bytes::copy_from_slice(&self.rbuf[self.rpos..self.rpos + n]);
        self.rpos += n;
        Ok(out)
    }

    /// 未消費のデータが無ければ 1 回受信する。戻り値は未消費のバイト数（0 は EOF）。
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

    /// 全部書く（書いたバイト数を返す）。
    pub async fn write(&mut self, data: &[u8]) -> Result<usize, L4Error> {
        self.write_all(data).await?;
        Ok(data.len())
    }

    /// 全部書く。
    pub async fn write_all(&mut self, data: &[u8]) -> Result<(), L4Error> {
        self.write_bytes(Bytes::copy_from_slice(data)).await
    }

    /// `Bytes` をコピーせずに全部書く。
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

    /// 送信側を閉じる（相手に EOF を知らせる）。
    pub fn shutdown_write(&mut self) -> Result<(), L4Error> {
        self.stream
            .shutdown(std::net::Shutdown::Write)
            .map_err(L4Error::Io)
    }
}

/// UDP ハンドラに渡すデータグラム。戻り値の `Some(bytes)` は送信元へ返信する。
#[derive(Debug, Clone)]
pub struct L4Datagram {
    pub data: Bytes,
    pub peer: SocketAddr,
    pub listener: Arc<str>,
}

/// TCP ハンドラ関数。
pub type L4TcpFn = Arc<
    dyn for<'a> Fn(&'a mut L4Connection) -> Pin<Box<dyn Future<Output = Result<(), L4Error>> + 'a>>
        + Send
        + Sync,
>;

/// UDP ハンドラ関数。
pub type L4UdpFn =
    Arc<dyn Fn(L4Datagram) -> Pin<Box<dyn Future<Output = Option<Bytes>>>> + Send + Sync>;

/// マクロ（`#[veil::l4_handler(protocol = "tcp")]`）の静的登録。
pub struct L4HandlerDef {
    pub name: &'static str,
    pub handler:
        for<'a> fn(&'a mut L4Connection) -> Pin<Box<dyn Future<Output = Result<(), L4Error>> + 'a>>,
}
inventory::collect!(L4HandlerDef);

/// マクロ（`#[veil::l4_handler(protocol = "udp")]`）の静的登録。
pub struct L4UdpHandlerDef {
    pub name: &'static str,
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

/// `App::l4_tcp` / `l4_udp` が注入する `[[l4]]` テーブル。
pub(crate) static INJECTED: Mutex<Vec<toml::Value>> = Mutex::new(Vec::new());

fn register(name: &str, h: L4Handler) -> Result<(), String> {
    let mut m = HANDLERS.write().unwrap();
    if m.contains_key(name) {
        return Err(format!("L4 handler '{}' is already registered", name));
    }
    m.insert(name.to_string(), h);
    Ok(())
}

/// マクロで定義した L4 ハンドラを登録する（何度呼んでも 1 回だけ）。
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

/// 名前でハンドラを引く（`[[l4]] handler` の解決）。
pub(crate) fn lookup(name: &str) -> Option<L4Handler> {
    HANDLERS.read().unwrap().get(name).cloned()
}

/// `[[l4]] handler` の名前が TCP / UDP のどちらで登録されているか（設定検証用）。
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
    /// TCP リスナー `listen` を開き、接続を `f` で処理する（`[[l4]]` を注入する）。
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

    /// UDP リスナー `listen` を開き、データグラムを `f` で処理する（`Some` を返すと送信元へ返信）。
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

/// 設定フック: `App::l4_*` の `[[l4]]` を注入する（同名の `[[l4]]` が TOML にあれば注入しない）。
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

/// TCP ハンドラで接続を処理する（`l4::server` の accept ループから呼ばれる）。
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

/// UDP ハンドラでリスナーを処理する（1 データグラムごとに呼ぶ。返信は同じソケットから）。
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
