//! F-87: io_uring Future のランダム Drop（キャンセル安全性）統合テスト
//!
//! io_uring はカーネルがユーザー空間バッファへ非同期に書き込むため、`.await` 中の
//! Future がいつでも Drop されうる（`timeout` / `select` / 接続切断）環境では、
//! Drop 後に遅延到着した CQE が解放済みバッファへ書き込まないこと（detach ガード +
//! 世代スラブ、B-07 の意味論）がクリティカルである。
//!
//! 本テストは実リング（実カーネル）上で I/O Future（recv / send / accept / timer）を
//! **ランダムなタイミングで強制 Drop** し続け、
//! - panic / abort が発生しない
//! - Drop 後もランタイムが生存し、後続の I/O が正しく完了する（liveness + 正当性）
//! ことを検証する。ASAN ビルド（F-85）と組み合わせると UAF を実行レベルで検出できる。
//!
//! 乱数は決定的 LCG。シードは `VEIL_CANCEL_TEST_SEED` で上書き可能（失敗再現用に
//! 標準エラーへ出力する）。io_uring を使えない環境（サンドボックス等）ではスキップする。

use std::io::{Read as _, Write as _};
use std::net::SocketAddr;
use std::time::Duration;

use veil::runtime::{self, time::timeout, TcpListener, TcpStream};

/// io_uring が利用可能か（生成できるか）。不可の環境ではテストをスキップする。
fn io_uring_available() -> bool {
    veil::runtime::IoUring::new(8, 0).is_ok()
}

/// 決定的な擬似乱数（LCG）。失敗再現のためシードを固定できる。
struct Lcg(u64);

impl Lcg {
    fn from_env() -> Self {
        let seed = std::env::var("VEIL_CANCEL_TEST_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0x5EED_F870_2026_0706u64);
        eprintln!("runtime_cancellation_test seed = {seed}");
        Lcg(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// サーバー: 接続ごとに「少し遅れてペイロードを送信 → クライアント切断まで drain」する。
/// 送信遅延をばらつかせることで、クライアント側の recv Future Drop が
/// 「提出直後（データ未着）」「完了直前」「完了後（CQE 到着済み）」の各タイミングに当たる。
fn spawn_payload_server(
    conns: usize,
    payload: &'static [u8],
) -> (SocketAddr, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let handle = std::thread::spawn(move || {
        let mut workers = Vec::new();
        for i in 0..conns {
            let Ok((mut s, _)) = listener.accept() else {
                break;
            };
            workers.push(std::thread::spawn(move || {
                // 接続ごとに 0〜300µs 程度の決定的なばらつき。
                std::thread::sleep(Duration::from_micros((i as u64 * 37) % 300));
                let _ = s.write_all(payload);
                let mut sink = [0u8; 1024];
                while matches!(s.read(&mut sink), Ok(n) if n > 0) {}
            }));
        }
        for w in workers {
            let _ = w.join();
        }
    });
    (addr, handle)
}

/// recv / send Future をランダムなタイミングで Drop し続けても、ランタイムが panic せず
/// 後続 I/O が正しく完了すること。
#[test]
fn random_drop_of_recv_send_futures_keeps_runtime_alive() {
    if !io_uring_available() {
        eprintln!("io_uring unavailable; skipping random_drop_of_recv_send_futures_keeps_runtime_alive");
        return;
    }

    const ITERATIONS: usize = 120;
    const PROBE_EVERY: usize = 8;
    static PAYLOAD: &[u8] = b"F-87 cancellation-safety payload: 0123456789abcdef";

    // 総接続数 = ランダム Drop 用 + 定期 liveness プローブ用。
    let probes = ITERATIONS / PROBE_EVERY;
    let (addr, server) = spawn_payload_server(ITERATIONS + probes, PAYLOAD);

    let mut rng = Lcg::from_env();

    runtime::block_on(async move {
        for i in 0..ITERATIONS {
            let stream = TcpStream::connect(addr).await.expect("connect");

            match rng.below(4) {
                // (a) 提出前 Drop: Future を作って一度も poll せずに捨てる。
                0 => {
                    let fut = stream.read(vec![0u8; 4096]);
                    drop(fut);
                }
                // (b) ランダム時間で recv を打ち切る（提出済み in-flight の Drop。
                //     ペイロード到着タイミングとの競合で「完了直前 / 完了後」も踏む）。
                1 => {
                    let micros = rng.below(800);
                    let _ = timeout(
                        Duration::from_micros(micros),
                        async { stream.read(vec![0u8; 4096]).await },
                    )
                    .await;
                }
                // (c) send を打ち切る（送信 in-flight の Drop。小さな送信は即完了しうる）。
                2 => {
                    let micros = rng.below(200);
                    let data = vec![b'x'; 32 * 1024];
                    let _ = timeout(
                        Duration::from_micros(micros),
                        async { stream.write(data).await },
                    )
                    .await;
                }
                // (d) recv を打ち切った直後に同一接続でもう一度 recv して Drop（多重キャンセル）。
                _ => {
                    let _ = timeout(
                        Duration::from_micros(rng.below(300)),
                        async { stream.read(vec![0u8; 1024]).await },
                    )
                    .await;
                    let _ = timeout(
                        Duration::from_micros(rng.below(300)),
                        async { stream.read(vec![0u8; 1024]).await },
                    )
                    .await;
                }
            }
            // stream Drop（in-flight が残っていれば detach + ASYNC_CANCEL 経路）。
            drop(stream);

            // 定期 liveness プローブ: 新規接続でペイロードを完全受信できること（正当性）。
            if (i + 1) % PROBE_EVERY == 0 {
                let probe = TcpStream::connect(addr).await.expect("probe connect");
                let mut received = Vec::new();
                while received.len() < PAYLOAD.len() {
                    let buf = vec![0u8; 4096];
                    let (res, buf) = timeout(Duration::from_secs(5), async {
                        probe.read(buf).await
                    })
                    .await
                    .expect("runtime must stay alive: probe read timed out");
                    let n = res.expect("probe read");
                    assert!(n > 0, "unexpected EOF before full payload");
                    received.extend_from_slice(&buf[..n]);
                }
                assert_eq!(
                    &received[..PAYLOAD.len()],
                    PAYLOAD,
                    "payload corruption after random future drops"
                );
            }
        }
    });

    server.join().expect("server join");
}

/// accept / timer Future をランダムなタイミングで Drop しても、リスナーとタイマーが
/// 生存し続けること（B-07 の「孤立 accept が接続を奪う」対策 = detach + ASYNC_CANCEL の検証）。
#[test]
fn random_drop_of_accept_and_timer_futures_keeps_listener_alive() {
    if !io_uring_available() {
        eprintln!(
            "io_uring unavailable; skipping random_drop_of_accept_and_timer_futures_keeps_listener_alive"
        );
        return;
    }

    const ROUNDS: usize = 40;
    let mut rng = Lcg::from_env();

    runtime::block_on(async move {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("local_addr");

        for _ in 0..ROUNDS {
            // (a) accept をランダム時間で打ち切る（in-flight ACCEPT の Drop）。
            let micros = rng.below(500);
            let _ = timeout(Duration::from_micros(micros), listener.accept()).await;

            // (b) timer Future の Drop: 長い sleep を短い timeout で打ち切る。
            let _ = timeout(
                Duration::from_micros(rng.below(400)),
                runtime::time::sleep(Duration::from_millis(5)),
            )
            .await;

            // (c) liveness: クライアントを接続し、accept が依然機能することを確認する。
            //     直前に Drop した accept のキャンセルが間に合わず接続を奪う競合があり得る
            //     ため（ガードが fd をクローズして接続は捨てられる）、数回リトライする。
            let mut accepted = false;
            for _ in 0..5 {
                let client = std::thread::spawn(move || {
                    let _ = std::net::TcpStream::connect(addr);
                });
                let got = timeout(Duration::from_secs(2), listener.accept()).await;
                client.join().expect("client join");
                if matches!(got, Ok(Ok(_))) {
                    accepted = true;
                    break;
                }
            }
            assert!(
                accepted,
                "listener must keep accepting after random accept-future drops"
            );
        }
    });
}
