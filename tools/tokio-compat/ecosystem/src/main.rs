//! tokio 互換レイヤー（F-198）の実ライブラリ試験。`ok <名前>` / `FAIL <名前>: <理由>` を出し、失敗があれば終了コード 1。
//! この crate の `tokio` は crates/veil-tokio/tokio-shim（veil のランタイム）に置き換わっている。

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

static mut FAILED: bool = false;

fn report(name: &str, r: Result<(), String>) {
    match r {
        Ok(()) => println!("ok   {}", name),
        Err(e) => {
            println!("FAIL {}: {}", name, e);
            unsafe { FAILED = true };
        }
    }
}

/// hyper のサーバ（hyper-util の auto: HTTP/1.1 と h2c）。
async fn start_hyper_server() -> std::io::Result<SocketAddr> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(|req: hyper::Request<hyper::body::Incoming>| async move {
                    let path = req.uri().path().to_string();
                    if path == "/slow" {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                    let body = req.into_body().collect().await.map(|b| b.to_bytes()).unwrap_or_default();
                    let text = format!("{} {}", path, body.len());
                    Ok::<_, Infallible>(hyper::Response::new(Full::new(Bytes::from(text))))
                });
                let _ = hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new())
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    Ok(addr)
}

async fn hyper_client(addr: SocketAddr) -> Result<(), String> {
    let stream = tokio::net::TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
        .await
        .map_err(|e| e.to_string())?;
    tokio::spawn(conn);
    for i in 0..20 {
        let req = hyper::Request::post(format!("http://{}/h1/{}", addr, i))
            .body(Full::new(Bytes::from(vec![b'x'; 1000 * i])))
            .unwrap();
        let resp = sender.send_request(req).await.map_err(|e| e.to_string())?;
        let body = resp.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
        let want = format!("/h1/{} {}", i, 1000 * i);
        if body != want.as_bytes() {
            return Err(format!("got {:?} want {}", body, want));
        }
    }
    Ok(())
}

async fn hyper_h2_client(addr: SocketAddr) -> Result<(), String> {
    let stream = tokio::net::TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
    let (sender, conn) = hyper::client::conn::http2::handshake(hyper_util::rt::TokioExecutor::new(), hyper_util::rt::TokioIo::new(stream))
        .await
        .map_err(|e| e.to_string())?;
    tokio::spawn(conn);
    let mut set = tokio::task::JoinSet::new();
    for i in 0..50 {
        let mut s = sender.clone();
        set.spawn(async move {
            let req = hyper::Request::get(format!("http://{}/h2/{}", addr, i)).body(Full::new(Bytes::new())).unwrap();
            let resp = s.send_request(req).await.map_err(|e| e.to_string())?;
            let body = resp.into_body().collect().await.map_err(|e| e.to_string())?.to_bytes();
            if body != format!("/h2/{} 0", i).as_bytes() {
                return Err(format!("bad body {:?}", body));
            }
            Ok::<_, String>(())
        });
    }
    while let Some(r) = set.join_next().await {
        r.map_err(|e| e.to_string())??;
    }
    Ok(())
}

async fn reqwest_client(addr: SocketAddr) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())?;
    let mut futs = Vec::new();
    for i in 0..30 {
        let c = client.clone();
        futs.push(async move {
            let r = c
                .post(format!("http://{}/rq/{}", addr, i))
                .body(vec![b'y'; i * 10])
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let t = r.text().await.map_err(|e| e.to_string())?;
            if t != format!("/rq/{} {}", i, i * 10) {
                return Err(t);
            }
            Ok::<_, String>(())
        });
    }
    for r in futures::future::join_all(futs).await {
        r?;
    }
    // タイムアウト（reqwest の tokio::time を使う）
    let quick = reqwest::Client::builder()
        .timeout(Duration::from_millis(100))
        .build()
        .map_err(|e| e.to_string())?;
    match quick.get(format!("http://{}/slow", addr)).send().await {
        Err(e) if e.is_timeout() => Ok(()),
        other => Err(format!("want timeout, got {:?}", other.map(|r| r.status()))),
    }
}

async fn tower_http_timeout() -> Result<(), String> {
    use tower::{ServiceBuilder, ServiceExt};
    let svc = ServiceBuilder::new()
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(http::StatusCode::REQUEST_TIMEOUT, Duration::from_millis(50)))
        .service_fn(|req: http::Request<Full<Bytes>>| async move {
            if req.uri().path() == "/slow" {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Ok::<_, Infallible>(http::Response::new(Full::new(Bytes::from_static(b"fast"))))
        });
    let r = svc.clone().oneshot(http::Request::get("/fast").body(Full::new(Bytes::new())).unwrap()).await.unwrap();
    if r.status() != 200 {
        return Err(format!("fast: {}", r.status()));
    }
    let r = svc.oneshot(http::Request::get("/slow").body(Full::new(Bytes::new())).unwrap()).await.unwrap();
    if r.status() != http::StatusCode::REQUEST_TIMEOUT {
        return Err(format!("slow: want 408 got {}", r.status()));
    }
    Ok(())
}

async fn sqlx_sqlite() -> Result<(), String> {
    use sqlx::Row;
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(4)
        .connect("sqlite::memory:")
        .await
        .map_err(|e| e.to_string())?;
    sqlx::query("create table t (id integer primary key, name text)")
        .execute(&pool)
        .await
        .map_err(|e| e.to_string())?;
    for i in 0..100 {
        sqlx::query("insert into t (name) values (?)")
            .bind(format!("n{}", i))
            .execute(&pool)
            .await
            .map_err(|e| e.to_string())?;
    }
    let row = sqlx::query("select count(*) as c from t").fetch_one(&pool).await.map_err(|e| e.to_string())?;
    let c: i64 = row.get("c");
    if c != 100 {
        return Err(format!("count {}", c));
    }
    Ok(())
}

async fn sqlx_postgres(url: &str) -> Result<(), String> {
    use sqlx::Row;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(10))
        .connect(url)
        .await
        .map_err(|e| e.to_string())?;
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..20i32 {
        let p = pool.clone();
        tasks.spawn(async move {
            let r = sqlx::query("select $1::int + 1 as v").bind(i).fetch_one(&p).await.map_err(|e| e.to_string())?;
            let v: i32 = r.get("v");
            if v != i + 1 {
                return Err(format!("{} -> {}", i, v));
            }
            Ok::<_, String>(())
        });
    }
    while let Some(r) = tasks.join_next().await {
        r.map_err(|e| e.to_string())??;
    }
    Ok(())
}

async fn sync_and_time() -> Result<(), String> {
    // 別スレッド（spawn_blocking）から mpsc・oneshot・Notify で起こす
    let (tx, mut rx) = tokio::sync::mpsc::channel::<u32>(4);
    let (otx, orx) = tokio::sync::oneshot::channel::<&'static str>();
    let notify = std::sync::Arc::new(tokio::sync::Notify::new());
    let n2 = notify.clone();
    tokio::task::spawn_blocking(move || {
        for i in 0..10 {
            tx.blocking_send(i).unwrap();
        }
        std::thread::sleep(Duration::from_millis(20));
        n2.notify_one();
        otx.send("done").unwrap();
    });
    let mut sum = 0;
    while let Some(v) = rx.recv().await {
        sum += v;
    }
    notify.notified().await;
    let msg = orx.await.map_err(|e| e.to_string())?;
    if sum != 45 || msg != "done" {
        return Err(format!("sum {} msg {}", sum, msg));
    }
    let m = std::sync::Arc::new(tokio::sync::Mutex::new(0));
    let mut hs = Vec::new();
    for _ in 0..50 {
        let m = m.clone();
        hs.push(tokio::spawn(async move {
            let mut g = m.lock().await;
            tokio::task::yield_now().await;
            *g += 1;
        }));
    }
    for h in hs {
        h.await.map_err(|e| e.to_string())?;
    }
    if *m.lock().await != 50 {
        return Err("mutex".into());
    }
    let t = std::time::Instant::now();
    let r = tokio::time::timeout(Duration::from_millis(30), tokio::time::sleep(Duration::from_secs(5))).await;
    if r.is_ok() || t.elapsed() > Duration::from_secs(1) {
        return Err("timeout".into());
    }
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(5)) => return Err("select picked the slow branch".into()),
        v = async { 7 } => if v != 7 { return Err("select".into()) },
    }
    let (a, b) = tokio::join!(async { 1 }, async { 2 });
    if a + b != 3 {
        return Err("join".into());
    }
    Ok(())
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    report("sync/time/select/join (cross-thread wakes)", sync_and_time().await);
    let addr = match start_hyper_server().await {
        Ok(a) => a,
        Err(e) => {
            report("hyper server", Err(e.to_string()));
            std::process::exit(1);
        }
    };
    report("hyper server + http1 client", hyper_client(addr).await);
    report("hyper h2c client (50 concurrent)", hyper_h2_client(addr).await);
    report("reqwest (30 concurrent + timeout)", reqwest_client(addr).await);
    report("tower-http TimeoutLayer", tower_http_timeout().await);
    report("sqlx sqlite", sqlx_sqlite().await);
    match std::env::var("VEIL_PG_URL") {
        Ok(url) => report("sqlx postgres (20 concurrent)", sqlx_postgres(&url).await),
        Err(_) => println!("skip sqlx postgres (VEIL_PG_URL not set)"),
    }
    if unsafe { FAILED } {
        std::process::exit(1);
    }
    // 接続を待っているタスクを残したまま runtime の中で process::exit しても abort しない
    let held = tokio::net::TcpStream::connect(addr).await;
    tokio::spawn(async move {
        if let Ok(mut s) = held {
            let mut b = [0u8; 1];
            let _ = tokio::io::AsyncReadExt::read(&mut s, &mut b).await;
        }
    });
    tokio::task::yield_now().await;
    println!("ok   process::exit inside the runtime with pending I/O");
    std::process::exit(0);
}
