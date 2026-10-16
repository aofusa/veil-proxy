//! 上流へのプロキシ（`runtime::tcp::TcpStream`）
//!
//! F-233 で `src/http3_server.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// 非同期バックエンドプロキシ（`runtime::tcp::TcpStream` 使用）
// ====================

/// バックエンドプロキシ結果
pub struct BackendProxyResult {
    /// HTTPステータスコード
    pub status_code: u16,
    /// レスポンスボディ
    pub body: Vec<u8>,
    /// レスポンスヘッダー（(name, value) のペア）
    pub headers: Vec<(Vec<u8>, Vec<u8>)>,
    /// HTTP/2 trailers（H2C/gRPC 用。H1 バックエンドでは空）
    pub trailers: Vec<(Vec<u8>, Vec<u8>)>,
}

/// B-38: HTTP/3 経路で WASM on_response_headers を適用する
#[cfg(feature = "wasm")]
pub(super) async fn apply_h3_wasm_response_headers(
    wasm_modules: &std::sync::Arc<Vec<crate::wasm_plugin_config::ModuleRef>>,
    status: u16,
    header_store: Vec<(Vec<u8>, Vec<u8>)>,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    if wasm_modules.is_empty() {
        return header_store;
    }
    let config = CURRENT_CONFIG.load();
    let Some(ref wasm_engine) = config.wasm_filter_engine else {
        return header_store;
    };

    let wasm_result = wasm_engine
        .clone()
        .on_response_headers_with_modules_async(
            wasm_modules.clone(),
            status,
            header_store.clone(),
            true,
        )
        .await;

    if let crate::wasm::FilterResult::Continue {
        headers: modified_headers,
        ..
    } = wasm_result
    {
        return modified_headers;
    }
    header_store
}

/// F-132: HTTP/3 経路の WASM ライフサイクル終端ヘルパ（`on_log` = `on_request_complete_async`）。
///
/// `LocalResponse` による早期 return を含む**すべての離脱点**から呼ぶことで、
/// HTTP/1.1・HTTP/2 経路（`src/proxy.rs`）と同じ回数だけ `proxy_on_log` が呼ばれるようにする。
/// モジュール未適用（`None` または空リスト）ならコストゼロで即 return する
/// （ホットパス絶対規則: WASM 未設定時は一切コストを増やさない）。
#[cfg(feature = "wasm")]
pub(super) async fn finish_h3_wasm_lifecycle(
    wasm_modules_to_apply: &Option<Arc<Vec<crate::wasm_plugin_config::ModuleRef>>>,
) {
    let Some(modules) = wasm_modules_to_apply else {
        return;
    };
    if modules.is_empty() {
        return;
    }
    let config = CURRENT_CONFIG.load();
    if let Some(ref wasm_engine) = config.wasm_filter_engine {
        crate::wasm::on_request_complete_async(wasm_engine.clone(), modules.clone()).await;
    }
}

/// B-39: HTTP/3 → H2C 上流プロキシ（gRPC 等。バッファ型）
///
/// F-174: 多重化した上流 HTTP/2 接続（ワーカーごとのプール）でストリームを開き、
/// レスポンスヘッダ + ボディ + trailers を返す（旧実装は 1 接続 1 ストリームの直列利用）。
#[cfg(feature = "http2")]
pub(super) async fn proxy_to_h2c_backend_async(
    target: &ProxyTarget,
    method: &[u8],
    path: &[u8],
    headers: &[(Vec<u8>, Vec<u8>)],
    request_body: &Bytes,
    timeout_secs: u64,
    security: &SecurityConfig,
) -> io::Result<BackendProxyResult> {
    // F-41/B-74/F-170: スタック整形（UDS バックエンドにも対応）。
    let addr = target.conn_addr();
    let key = crate::http_utils::PoolKeyStr::plain_addr(addr.as_str());
    let body = (!request_body.is_empty()).then(|| request_body.clone());
    let response = crate::http2::upstream_mux::request_h2c_buffered(
        target,
        key.as_str(),
        Duration::from_secs(security.backend_connect_timeout_secs),
        Duration::from_secs(security.idle_connection_timeout_secs),
        Duration::from_secs(timeout_secs),
        method,
        path,
        target.host.as_bytes(),
        headers.iter().map(|(k, v)| (k.as_slice(), v.as_slice())),
        body,
        crate::http_utils::is_idempotent_method(method),
    )
    .await
    .map_err(|status| {
        warn!(
            "[HTTP/3] H2C request to {} failed: {}",
            addr.as_str(),
            status
        );
        if status == 504 {
            io::Error::new(io::ErrorKind::TimedOut, "H2C request timeout")
        } else {
            io::Error::other("H2C request failed")
        }
    })?;

    debug!(
        "[HTTP/3] H2C response: status={} body_len={} trailers={}",
        response.status,
        response.body.len(),
        response.trailers.len()
    );

    Ok(BackendProxyResult {
        status_code: response.status,
        // `BackendProxyResult` は `Vec<u8>` 型（境界で変換。`Bytes` が一意参照ならコピー無し）。
        body: Vec::from(response.body),
        headers: response
            .headers
            .into_iter()
            .map(|(k, v)| (Vec::from(k), Vec::from(v)))
            .collect(),
        trailers: response
            .trailers
            .into_iter()
            .map(|(k, v)| (Vec::from(k), Vec::from(v)))
            .collect(),
    })
}

/// コネクション管理（`Rc<RefCell>` で共有）
/// 接続マップ本体。キーはサーバが HMAC で導出した接続 ID（B-94）で、データグラムごとに
/// 数回引くため、既定の SipHash ではなく乱数シード付きの foldhash を使う
/// （FreeBSD aarch64 の HTTP/3 プロファイルで SipHash が上位だった）。挿入されるキーは
/// 予測不能な HMAC 出力なので、シード付きの高速ハッシュで HashDoS の心配は無い。
///
/// F-186: 値は `Box<Http3Handler>`。ハンドラは quiche の接続を値で持つ数 KB の構造体で、値のまま表に置くと
/// 表の容量の倍増（再ハッシュ）のたびに全ハンドラを移動し、同時接続数のピークに合わせて表が
/// ワーカーあたり 16MB まで伸びたまま残っていた。Box の確保は接続の確立ごとに 1 回（要求ごとではない）。
pub(super) type ConnMapInner =
    HashMap<ConnectionId<'static>, Box<Http3Handler>, foldhash::quality::RandomState>;

/// F-186: 接続表の容量がこの値以下なら縮めない（小さい表の伸縮を繰り返さない）。
pub(super) const CONN_MAP_SHRINK_MIN_CAPACITY: usize = 256;

/// F-186: アイドル点（受信待ちのタイムアウト）で、接続が減って表の容量が 4 倍以上余っていたら縮める。
/// 要求ごとの経路からは呼ばない（`alloc_tune::idle_tick` と同じ位置だけ）。
pub(super) fn shrink_idle_conn_map(conns: &mut ConnMapInner) {
    let cap = conns.capacity();
    if cap > CONN_MAP_SHRINK_MIN_CAPACITY && conns.len() * 4 < cap {
        conns.shrink_to((conns.len() * 2).max(CONN_MAP_SHRINK_MIN_CAPACITY));
    }
}

pub(super) type ConnectionMap = Rc<RefCell<ConnMapInner>>;

/// 接続マップのハッシャ（foldhash がプロセスごとの乱数シードを持つ）。
pub(super) fn conn_map_hasher() -> foldhash::quality::RandomState {
    foldhash::quality::RandomState::default()
}

/// HTTP/3 サーバーを起動（独自ランタイム上で実行）
///
/// この関数はランタイムのワーカースレッド（`runtime::block_on` の中）から呼び出す必要があります。
/// HTTP/1.1と同等のルーティング・セキュリティ・プロキシ機能をサポートします。
///
/// ## セキュリティ
/// 証明書データ（cert_pem, key_pem）は quiche へのロード完了後、
/// セキュアにゼロ化してからメモリから解放されます。
// clippy::await_holding_refcell_ref 許容理由: `connections`（Rc<RefCell<HashMap>>）を
// 借用するのは本 H3 メインループタスクのみ。バックエンドタスクは Rc チャネル + Notify
// 経由で通信し RefCell に触れない（F-32 のアクターモデル）ため、await 中に他タスクが
// 再入借用して panic する経路は存在しない（B-16 とは異なり単一借用者）。
// F-149: 以下 7 種の起動時ログ（証明書ロード方式/トランスポートパラメータ/GSO・GRO/
// リスンアドレス/パイプライン化 RECVMSG・SENDMSG）は `run_http3_server_async` の
// 起動処理部分（ループに入る前）でワーカースレッドごとに実行されるが、内容はワーカー間で
// 完全に同一のため最初の 1 回だけ出力する（ログ行ごとに個別の `Once` を用意する。単一の
// `Once` を複数の `call_once` 呼び出しで共有すると、最初に完了した呼び出しの後は他の
// クロージャが一切実行されなくなるため不可）。
//
// `Once` は起動経路とリロード経路が同じ関数を共有していない場合のみ安全に使える
// （共有している場合、リロード時に再度この経路を通ると 2 回目以降が出力されなくなって
// しまう）。本関数のうち Once で囲む区間は「関数がループに入る前の初回セットアップ」のみで、
// 証明書ホットリロードは同じ関数内の別区間ではなく別関数 `reload_quiche_certs`
// （メインループの中から呼ばれる）が担う。つまりリロードはこの Once 区間へ再入しないため、
// 「プロセス全体で 1 回」のまま安全に使える。
pub(super) static HTTP3_LOG_ONCE_CERT_LOADING: Once = Once::new();
pub(super) static HTTP3_LOG_ONCE_CERT_LOADED: Once = Once::new();
pub(super) static HTTP3_LOG_ONCE_TRANSPORT: Once = Once::new();
pub(super) static HTTP3_LOG_ONCE_GSO_GRO: Once = Once::new();
pub(super) static HTTP3_LOG_ONCE_LISTENING: Once = Once::new();
// パイプライン化 io_uring RECVMSG/SENDMSG のログは Linux uring バックエンドでのみ
// 存在する分岐（`#[cfg(all(target_os = "linux", veil_rt_uring))]`）の中でのみ使うため、
// 他バックエンド（reactor/epoll）ビルドで未使用 static にならないよう同じ cfg を付ける。
#[cfg(all(target_os = "linux", veil_rt_uring))]
pub(super) static HTTP3_LOG_ONCE_RECVMSG: Once = Once::new();
#[cfg(all(target_os = "linux", veil_rt_uring))]
pub(super) static HTTP3_LOG_ONCE_SENDMSG: Once = Once::new();

#[allow(clippy::await_holding_refcell_ref)]
pub async fn run_http3_server_async(
    bind_addr: SocketAddr,
    mut config: Http3ServerConfig,
) -> io::Result<()> {
    // TLS 証明書を設定した QUIC 設定を作成する（F-136）。
    //
    // - Linux: `new_quic_config_with_certs` が memfd 経由でロードする
    //   （`/proc/self/fd/<fd>`、Landlock でファイルシステムアクセスを制限しながら
    //   HTTP/3 を使用可能）。
    // - それ以外（非 Linux）: PEM バイト列から直接 BoringSSL の
    //   `SslContextBuilder` を組む in-memory 経路（capsicum capability mode /
    //   pledge+unveil 下でも動作する）。
    //
    // セキュリティ: quiche が証明書を読み込んだ後、config 内の Vec<u8> をセキュアに
    // ゼロ化してからドロップする。
    let mut quic_config = if let (Some(mut cert_pem), Some(mut key_pem)) =
        (config.cert_pem.take(), config.key_pem.take())
    {
        HTTP3_LOG_ONCE_CERT_LOADING.call_once(|| {
            info!(
                "[HTTP/3] Loading certificates ({})",
                if cfg!(target_os = "linux") {
                    "via memfd/temp file (path-based quiche API)"
                } else {
                    "in-memory SSL_CTX, capability-mode compatible"
                }
            );
        });

        let cfg = new_quic_config_with_certs(&cert_pem, &key_pem)?;

        // 証明書・秘密鍵データをセキュアにゼロ化
        secure_zero(&mut cert_pem);
        drop(cert_pem);
        secure_zero(&mut key_pem);
        drop(key_pem);
        debug!("[HTTP/3] Certificate/key data securely zeroed and released");
        HTTP3_LOG_ONCE_CERT_LOADED
            .call_once(|| info!("[HTTP/3] Certificates loaded, sensitive data zeroed"));

        cfg
    } else {
        // ファイルパスから直接ロード（後方互換性）
        info!("[HTTP/3] Loading certificates from file path (legacy mode)");
        #[cfg(target_os = "linux")]
        {
            warn!("[HTTP/3] Note: When using Landlock, add cert/key paths to landlock_read_paths");
            let mut cfg = Config::new(quiche::PROTOCOL_VERSION)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
            cfg.load_cert_chain_from_pem_file(&config.cert_path)
                .map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("cert load error: {}", e),
                    )
                })?;
            cfg.load_priv_key_from_pem_file(&config.key_path)
                .map_err(|e| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("key load error: {}", e),
                    )
                })?;
            cfg
        }
        #[cfg(not(target_os = "linux"))]
        {
            // 理由付き allow: 起動時コールドパス（cert_pem 未指定のレガシー経路のみ。
            // 通常経路は config.rs が Landlock 対応済みの PEM バイト列を事前に読み込んで渡す）。
            #[allow(clippy::disallowed_methods)]
            let cert_pem = std::fs::read(&config.cert_path).map_err(|e| {
                io::Error::new(e.kind(), format!("failed to read cert file: {}", e))
            })?;
            #[allow(clippy::disallowed_methods)]
            let key_pem = std::fs::read(&config.key_path)
                .map_err(|e| io::Error::new(e.kind(), format!("failed to read key file: {}", e)))?;
            new_quic_config_with_certs(&cert_pem, &key_pem)?
        }
    };

    // QUIC トランスポートパラメータを設定（初回ロード・リロード共通の独立関数、F-136）
    configure_quic_transport(&mut quic_config, &config)?;
    HTTP3_LOG_ONCE_TRANSPORT.call_once(|| {
        info!(
            "[HTTP/3] quiche transport: cc={} pacing={} hystart={} mmsg_batch={} recv_drain_max={}",
            config.cc_algorithm.trim(),
            config.pacing,
            config.hystart,
            config.mmsg_batch_size,
            config.recv_drain_max
        );
    });

    // 設定を Rc で共有（quiche::Config は Clone できないため）
    let quic_config = Rc::new(RefCell::new(quic_config));

    // F-105: 証明書ホットリロード。本ワーカーを登録し、現在の配信世代をローカルに控える。
    // 起動直後は上で cert/key をロード済みなので、ローカル世代を現在値に合わせて即時リロードを避ける。
    crate::tls_reload::register_http3_worker();
    let mut local_cert_gen = crate::tls_reload::http3_cert_generation();

    // UDP ソケットを作成（独自ランタイム上、`src/runtime/` のバックエンド経由）
    // SO_REUSEPORT を設定して複数ワーカーで並列処理を可能に
    // GSO/GRO は config.gso_gro_enabled に基づいて設定
    let socket = QuicUdpSocket::bind_reuseport_with_gso(bind_addr, config.gso_gro_enabled)?;
    HTTP3_LOG_ONCE_GSO_GRO.call_once(|| {
        info!(
            "[HTTP/3] GSO enabled: {}, GRO enabled: {} (config gso_gro_enabled: {})",
            socket.gso_enabled(),
            socket.gro_enabled(),
            config.gso_gro_enabled
        );
    });
    let socket = Rc::new(socket);
    let local_addr = bind_addr;

    // F-149(B.9): F-28 より前の外部ランタイムの名前ではなく、ビルドバックエンドに応じて実態の
    // ランタイム名（io_uring / readiness reactor）を報告する（build.rs 発行の cfg、
    // veil_rt_uring = Linux 既定 io_uring、veil_rt_reactor = epoll/kqueue readiness）。
    #[cfg(veil_rt_uring)]
    const HTTP3_RUNTIME_BACKEND: &str = "io_uring";
    #[cfg(veil_rt_reactor)]
    const HTTP3_RUNTIME_BACKEND: &str = "readiness reactor";

    HTTP3_LOG_ONCE_LISTENING.call_once(|| {
        info!(
            "[HTTP/3] Server listening on {} (QUIC/UDP, {})",
            bind_addr, HTTP3_RUNTIME_BACKEND
        );
    });

    // コネクション管理
    let connections: ConnectionMap = Rc::new(RefCell::new(HashMap::with_hasher(conn_map_hasher())));

    // F-32: バックエンドタスク → メインループの起床通知（全ハンドラ/タスクで共有）。
    let notify = crate::http3_stream::H3Notify::new();
    // F-46: バックエンドタスクの型付きプール（本ワーカースレッドの全接続で共有）。
    let backend_spawner = crate::http3_stream::backend_task_spawner();
    // B-97: バッファ経路の要求 Future（静的配信・gRPC・Wasm・buffering=full 等）。メインループは
    // これを await せず、起床したものだけを select のアームと `poll_buffered_once` で進める。
    let mut buffered_futs = futures::stream::FuturesUnordered::new();

    // B-94: サーバ接続 ID 導出鍵（プロセスで 1 個。初回アクセス時に乱数で生成）。
    let cid_key: &hmac::Key = &SERVER_CID_KEY;

    // ルーティング設定を CURRENT_CONFIG から取得（ホットリロード対応）

    // F-33: 受信バッファを loop 外で一度だけ確保し再利用する（reactor / multishot 未使用時の
    // フォールバック経路用）。64KB は単一 recvmsg の最大値。
    let mut recv_buf = vec![0u8; 65536];

    // F-115 第2段 / F-124: recvmmsg 用スクラッチ（reactor フォールバック・drain 補助）。
    // バッチ幅は `[http3].mmsg_batch_size`（既定 64）。
    let mmsg_batch = crate::udp::socket::clamp_mmsg_batch(config.mmsg_batch_size);
    // F-152: reactor 経路の 1 イテレーションあたり drain 上限（`[http3] recv_drain_max`）。
    // io_uring 経路（`PipelinedUdpRecv`）は参照しないため、そちらのビルドでは未使用になる。
    let recv_drain_max = config.recv_drain_max.clamp(1, H3_RECV_DRAIN_MAX_LIMIT);
    let mut mmsg_scratch = crate::udp::socket::MmsgRecvScratch::with_batch(mmsg_batch);

    // F-130 C1/C2: io_uring RECVMSG バックエンド（Linux uring バックエンド）。
    // 既定は C1（パイプライン化 RECVMSG）。C2（真の IORING_RECV_MULTISHOT + provided
    // buffer ring）は **`VEIL_H3_BUFRING=1` を指定したときだけ**試み、非対応環境では
    // 自動的に C1 へフォールバックする（既定をオプトインにしている理由は
    // `UdpRecvBackend::new` の doc コメント参照）。
    // C1/C2 いずれも使わない場合は POLL+recvmmsg へフォールバックする。
    // デバッグで両方無効化する場合は VEIL_H3_MULTISHOT=0（環境変数名は F-124/F-129 からの互換名）。
    #[cfg(all(target_os = "linux", veil_rt_uring))]
    let mut ms_recv = {
        let disabled = std::env::var_os("VEIL_H3_MULTISHOT")
            .map(|v| v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"))
            .unwrap_or(false);
        if disabled {
            info!("[HTTP/3] io_uring RECVMSG backends disabled via VEIL_H3_MULTISHOT=0");
            None
        } else {
            match crate::runtime::udp_recv::UdpRecvBackend::new(socket.as_raw_fd(), mmsg_batch) {
                Ok(m) => {
                    let label = if m.is_multishot() {
                        "true multishot + buffer ring (C2)"
                    } else {
                        "pipelined RECVMSG (C1)"
                    };
                    HTTP3_LOG_ONCE_RECVMSG.call_once(|| {
                        info!(
                            "[HTTP/3] UDP receive backend: {} ({} buffers, no libc recvmmsg on hot path)",
                            label,
                            m.batch_size()
                        );
                    });
                    Some(m)
                }
                Err(e) => {
                    warn!(
                        "[HTTP/3] io_uring RECVMSG backends unavailable ({}), using POLL+recvmmsg fallback",
                        e
                    );
                    None
                }
            }
        }
    };

    // F-130 C3: パイプライン化 io_uring SENDMSG。受信側と同じ有効/無効判断（disabled 判定）に
    // 揃え、`VEIL_H3_MULTISHOT=0` で送信側も従来 sendmmsg 経路へ揃って戻す。
    #[cfg(all(target_os = "linux", veil_rt_uring))]
    {
        if ms_recv.is_some() {
            put_uring_udp_send(crate::runtime::udp_send::UringUdpSend::new(
                socket.as_raw_fd(),
                mmsg_batch,
            ));
            HTTP3_LOG_ONCE_SENDMSG.call_once(|| {
                info!(
                    "[HTTP/3] pipelined io_uring SENDMSG enabled ({} slots, no libc sendmmsg on hot path)",
                    mmsg_batch
                );
            });
        }
    }

    // F-151: ダーティ接続集合 + タイマー最小ヒープでイベント駆動化する（実験で確定した
    // 「1 イテレーションあたり接続マップ全体を最大 6 回スイープする固定費」を排除する）。
    // いずれもループ外で 1 度だけ確保し、以降は使い回す（ホットパスでの新規アロケーション
    // 禁止のため `VecDeque`/`BinaryHeap` を毎イテレーション `new()` しない）。
    // `ConnKey`（= `Rc<ConnectionId<'static>>`）で持ち回ることで、これらのキューへの
    // push のたびに発生していた `ConnectionId`（内部 `Vec<u8>`）のヒープ確保を
    // `Rc::clone`（参照カウント +1 のみ）に置き換える（F-151 レビュー修正）。
    let mut dirty_queue: VecDeque<ConnKey> = VecDeque::new();
    // F-187: graceful shutdown の期限（SHUTDOWN_FLAG を最初に見たときに決める）。
    let mut shutdown_deadline: Option<Instant> = None;
    let mut timers: BinaryHeap<Reverse<TimerKey>> = BinaryHeap::new();
    // F-151: バックエンドタスクが per-connection に積む起床キュー（`Http3Handler` 生成時に
    // `ConnWaker` へ `Rc` で共有する）。
    let wake_queue: crate::http3_stream::WakeQueue = Rc::new(RefCell::new(VecDeque::new()));
    // F-151: 末尾の送出スイープで走査する対象 cid（このイテレーションで処理した接続）。
    // ループ外で 1 度だけ確保し、毎イテレーション clear して使い回す。
    let mut send_targets: Vec<ConnKey> = Vec::new();

    // メインループ: パケット受信とディスパッチ
    loop {
        // シャットダウンチェック（F-187: 停止中もメインループを回し続ける。以前は受信も送信も
        // 止めて quiche のアイドルタイムアウトを待つだけで、GOAWAY も送っていなかった）。
        // 各接続に GOAWAY を送って新しい要求を断り、処理中の要求が終わった接続から
        // CONNECTION_CLOSE を出す。全接続が閉じるか `graceful_shutdown_timeout_secs` で抜ける。
        if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
            let now = Instant::now();
            let mut conns = connections.borrow_mut();
            let deadline = *shutdown_deadline.get_or_insert_with(|| {
                info!(
                    "[HTTP/3] Initiating graceful shutdown ({} connections)",
                    conns.len()
                );
                now + Duration::from_secs(
                    crate::config::GRACEFUL_SHUTDOWN_TIMEOUT_SECS.load(Ordering::Relaxed),
                )
            });
            if conns.is_empty() {
                info!("[HTTP/3] All connections drained");
                info!("[HTTP/3] Shutdown complete");
                break Ok(());
            }
            if now >= deadline {
                warn!(
                    "[HTTP/3] Drain timeout, {} connections still active",
                    conns.len()
                );
                info!("[HTTP/3] Shutdown complete");
                break Ok(());
            }
            for handler in conns.values_mut() {
                if handler.shutdown_step() {
                    mark_dirty_flag(&mut handler.dirty, &mut dirty_queue, &handler.key);
                }
            }
        }

        // F-105: 証明書ホットリロードの検知（安価な世代ゲート）。
        // 毎周回 u64 の atomic load を 1 回行うだけ（x86 では Relaxed 同等コスト）。世代が
        // 変わっていなければ ArcSwap には触れず即座に抜ける。SIGHUP で TLS リロードスレッドが
        // `publish_http3_certs` を呼ぶと世代が進み、ここで新 cert/key を quiche へ反映する。
        {
            let cur_gen = crate::tls_reload::http3_cert_generation();
            if cur_gen != local_cert_gen {
                if let Some(material) = crate::tls_reload::load_http3_material() {
                    match reload_quiche_certs(&quic_config, &material, &config) {
                        Ok(()) => {
                            info!(
                                "[HTTP/3] Certificate hot-reloaded (generation {})",
                                material.generation()
                            );
                        }
                        Err(e) => {
                            error!("[HTTP/3] Certificate hot-reload failed: {}", e);
                        }
                    }
                    // 適用完了を通知（最後のワーカーが平文をゼロ化）。失敗時も次回配信まで
                    // 再試行しないよう完了扱いにし、平文が滞留しないようにする。
                    material.worker_applied();
                    local_cert_gen = material.generation();
                } else {
                    // マテリアル未格納（想定外）。世代だけ追従して無限ループを避ける。
                    local_cert_gen = cur_gen;
                }
            }
        }

        // F-151: 次に処理すべき最短期限をタイマーヒープから算出する（全接続の
        // `conn.timeout()` を毎回スイープしていた従来方式を廃止）。
        // `!dirty_queue.is_empty()` を渡す: 前イテレーションで `did_work` により再投入された
        // 未処理の接続が残っている間は、タイマー期限に関係なく sleep せず即座に次の
        // ダーティ処理へ進む（レビュー修正: これをしないとストリーミング中のレスポンスが
        // チャンクごとに最大 100ms 停止し得る）。
        let now = Instant::now();
        let timeout_duration = next_sleep_duration(
            timers.peek().map(|Reverse(TimerKey(dl, _))| *dl),
            now,
            !dirty_queue.is_empty(),
        );

        // パケット受信・バックエンドタスク通知・タイムアウトの 3 者を多重化（F-32 + F-124）。
        //
        // **F-124 (io_uring)**: `IORING_OP_RECVMSG` + `IORING_RECV_MULTISHOT` + provided
        // buffers。POLL_ADD + 同期 recvmmsg の二重往復を廃し、CQE 経由でデータグラムを
        // 取り出す。ペイロードは提供バッファ内スライスを quiche 低レベル `recv` へ直渡し。
        //
        // **フォールバック (reactor / multishot 不可)**: 従来の recv_gro_async + recvmmsg drain。
        // F-32: バックエンド notify でメインループを起こしストリーミングを駆動する。

        // F-151: タイムアウト処理・起床キューの drain は受信 select の直後、両分岐で共通して
        // 1 回だけ行う（下で分岐後に実行）。

        #[cfg(all(target_os = "linux", veil_rt_uring))]
        let used_multishot = ms_recv.is_some();
        #[cfg(not(all(target_os = "linux", veil_rt_uring)))]
        let used_multishot = false;

        if used_multishot {
            #[cfg(all(target_os = "linux", veil_rt_uring))]
            {
                let ms = ms_recv.as_mut().expect("multishot present");
                enum MsOutcome {
                    Batch(io::Result<usize>),
                    Notified,
                    Timeout,
                }
                let outcome = futures::select_biased! {
                    r = futures::FutureExt::fuse(ms.recv_batch()) => MsOutcome::Batch(r),
                    _ = futures::FutureExt::fuse(std::future::poll_fn(|cx| poll_buffered_arm(&mut buffered_futs, cx))) => MsOutcome::Notified,
                    _ = futures::FutureExt::fuse(notify.wait()) => MsOutcome::Notified,
                    _ = futures::FutureExt::fuse(crate::runtime::time::sleep(timeout_duration)) => MsOutcome::Timeout,
                };

                // F-151: タイマーヒープから期限到来分だけ pop して on_timeout する。
                {
                    let mut conns = connections.borrow_mut();
                    expire_due_timers(&mut conns, &mut timers, &mut dirty_queue, Instant::now());
                    drain_wake_queue(&wake_queue, &mut conns, &mut dirty_queue);
                }

                if let MsOutcome::Timeout = outcome {
                    // F-186: 接続が減った後の接続表の余った容量を返す（アロケータへ返す前に）。
                    shrink_idle_conn_map(&mut connections.borrow_mut());
                    // F-183: アイドル点でアロケータに空き領域を OS へ返させる（1 秒に間引く）。
                    crate::alloc_tune::idle_tick();
                }
                if let MsOutcome::Batch(result) = outcome {
                    match result {
                        Ok(n) => {
                            let mut conns = connections.borrow_mut();
                            // F-130 C1: recv_batch() が今回完了を見つけた全スロット
                            // （n 件、複数同時のことがある）を処理する。libc recvmmsg は
                            // ホットパスに登場しない（完了データはすべて RECVMSG の CQE 由来）。
                            for i in 0..n {
                                let idx = ms.ready_slot(i);
                                match ms.take_result(idx) {
                                    Ok(meta) => {
                                        let payload = ms.payload_mut(idx, meta.payload_len);
                                        process_datagram_segments(
                                            &mut conns,
                                            payload,
                                            meta.from,
                                            meta.gro_segment_size,
                                            cid_key,
                                            &quic_config,
                                            local_addr,
                                            &notify,
                                            &backend_spawner,
                                            &wake_queue,
                                            &mut dirty_queue,
                                        )?;
                                    }
                                    Err(e) if e.kind() != io::ErrorKind::WouldBlock => {
                                        error!("[HTTP/3] RECVMSG error: {}", e);
                                    }
                                    Err(_) => {}
                                }
                            }
                            if let Err(e) = ms.rearm_ready() {
                                error!("[HTTP/3] RECVMSG re-arm failed: {}", e);
                            }
                            // F-151: 受信直後の送出は行わない（イテレーション末尾の 1 回に統一）。
                        }
                        Err(e) => {
                            error!("[HTTP/3] io_uring RECVMSG error: {}", e);
                            // F-130 C2: buffer ring 登録には対応するが true multishot recv
                            // 自体は未対応というカーネルギャップ（5.19〜6.0）を実行時に検出
                            // した場合、同じ fd で C1 へ 1 度だけ切り替える。
                            match ms.downgrade_to_pipelined_if_einval(mmsg_batch) {
                                Ok(true) => {
                                    warn!(
                                        "[HTTP/3] switched UDP receive backend to pipelined RECVMSG (C1) after runtime EINVAL"
                                    );
                                }
                                Ok(false) => {}
                                Err(downgrade_err) => {
                                    error!(
                                        "[HTTP/3] fallback to pipelined RECVMSG (C1) failed: {}",
                                        downgrade_err
                                    );
                                }
                            }
                        }
                    }
                }
            }
        } else {
            // フォールバック: POLL_ADD + recvmsg/recvmmsg（F-33/F-115）
            let recv_outcome = futures::select_biased! {
                r = futures::FutureExt::fuse(socket.recv_gro_async(&mut recv_buf)) => RecvOutcome::Packet(r),
                _ = futures::FutureExt::fuse(std::future::poll_fn(|cx| poll_buffered_arm(&mut buffered_futs, cx))) => RecvOutcome::Notified,
                _ = futures::FutureExt::fuse(notify.wait()) => RecvOutcome::Notified,
                _ = futures::FutureExt::fuse(crate::runtime::time::sleep(timeout_duration)) => RecvOutcome::Timeout,
            };

            {
                let mut conns = connections.borrow_mut();
                expire_due_timers(&mut conns, &mut timers, &mut dirty_queue, Instant::now());
                drain_wake_queue(&wake_queue, &mut conns, &mut dirty_queue);
            }

            let gro_result = match recv_outcome {
                RecvOutcome::Packet(Ok(r)) => Some(r),
                RecvOutcome::Packet(Err(e)) => {
                    if e.kind() != io::ErrorKind::WouldBlock {
                        error!("[HTTP/3] recv_gro error: {}", e);
                    }
                    None
                }
                RecvOutcome::Notified => None,
                RecvOutcome::Timeout => {
                    // F-186: 接続が減った後の接続表の余った容量を返す（アロケータへ返す前に）。
                    shrink_idle_conn_map(&mut connections.borrow_mut());
                    // F-183: アイドル点でアロケータに空き領域を OS へ返させる（1 秒に間引く）。
                    crate::alloc_tune::idle_tick();
                    None
                }
            };

            if let Some(first_gro) = gro_result {
                let mut conns = connections.borrow_mut();
                let first_total = first_gro.bytes_received;
                process_datagram_segments(
                    &mut conns,
                    &mut recv_buf[..first_total],
                    first_gro.from,
                    first_gro.gro_segment_size,
                    cid_key,
                    &quic_config,
                    local_addr,
                    &notify,
                    &backend_spawner,
                    &wake_queue,
                    &mut dirty_queue,
                )?;

                let mut drained = 0usize;
                while drained < recv_drain_max {
                    let n = match socket.recv_mmsg_sync(&mut mmsg_scratch) {
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    for i in 0..n {
                        let (len, from, gro) = match mmsg_scratch.meta(i) {
                            Ok(v) => v,
                            Err(e) => {
                                warn!("[HTTP/3] recvmmsg meta error: {}", e);
                                continue;
                            }
                        };
                        process_datagram_segments(
                            &mut conns,
                            &mut mmsg_scratch.buf_mut(i)[..len],
                            from,
                            gro,
                            cid_key,
                            &quic_config,
                            local_addr,
                            &notify,
                            &backend_spawner,
                            &wake_queue,
                            &mut dirty_queue,
                        )?;
                    }
                    drained += n;
                    if n < mmsg_scratch.batch_size() {
                        break;
                    }
                }
                // F-151: 受信直後の送出は行わない（イテレーション末尾の 1 回に統一）。
            }
        }

        // F-151: ダーティ接続だけを 1 回処理する（B-12: パケット受信時だけでなく通知/
        // タイマー起床時も含め、ダーティな接続は毎回 init_h3/handle_writable_streams/
        // process_h3_events/drive_proxy_streams を通す）。
        //
        // `drive_proxy_streams` の `recv_body`（`h3.poll()` の外）がストリームを進めると、
        // h3 イベントは poll でしか取り出せない形で滞留する。具体例（B-12 のハング）:
        // h3 クライアント（hyperium h3）は fin 直前に GREASE フレームを送るため、pump の
        // `recv_body` が最終 DATA を消費してもフレームペイロードが未読で残り
        // （非 DATA フレームの消費は poll 専用）、`Finished` も生成されない。クライアントは
        // 送信完了後は無通信のため「パケット到着時のみ poll」だと永久に取り残され、
        // EOF 未伝播 → レスポンス無し → QUIC アイドルタイムアウトの双方向デッドロックに陥る。
        //
        // **不変条件（B-12 再発防止）**: `handle_writable_streams` /
        // `process_h3_events` / `drive_proxy_streams` の**いずれか 1 件でも仕事をしたら**
        // 当該接続をダーティのままキューへ再投入し、次イテレーションでもう一度見る。
        // 「イベントが残っているのにダーティを降ろす」経路を構造的に排除するため。
        send_targets.clear();
        {
            let mut conns = connections.borrow_mut();
            // このイテレーション開始時点のキュー長だけ処理する。処理中に did_work で
            // 再投入された分は次イテレーションへ回す（無限ループ防止）。
            let n = dirty_queue.len();
            for _ in 0..n {
                let Some(cid) = dirty_queue.pop_front() else {
                    break;
                };
                // `ConnKey`（`Rc<ConnectionId>`）を `&*cid` で deref し、HashMap キー
                // （素の `ConnectionId<'static>`）としてルックアップする。
                let Some(handler) = conns.get_mut(&*cid) else {
                    continue; // 既に削除済み（タイムアウト/エラーでクローズ）。
                };

                let mut did_work = false;

                // HTTP/3 初期化
                if handler.h3_conn.is_none() && handler.conn.is_established() {
                    debug!("[HTTP/3] Connection established, initializing H3");
                }
                if let Err(e) = handler.init_h3() {
                    warn!("[HTTP/3] init_h3 error: {}", e);
                }

                // 書き込み可能なストリームを処理（quiche パターン）。部分レスポンスを再送する。
                if handler.h3_conn.is_some() {
                    match handler.handle_writable_streams() {
                        Ok(w) => did_work |= w,
                        Err(e) => warn!("[HTTP/3] handle_writable_streams error: {}", e),
                    }
                }

                // HTTP/3 イベント処理
                if handler.h3_conn.is_some() {
                    match handler.process_h3_events() {
                        Ok(w) => did_work |= w,
                        Err(e) => warn!("[HTTP/3] process_h3_events error: {}", e),
                    }
                }

                // B-97: 新しいバッファ経路の要求を Future 集合へ移し、その場で 1 回 poll する
                // （await せずに完了する要求は、直後の drive_proxy_streams で同じイテレーション内に
                // 送出される）。
                if !handler.new_buffered.is_empty() {
                    for (task, headers, body) in handler.new_buffered.drain(..) {
                        buffered_futs.push(buffered_request_task(task, headers, body));
                    }
                    poll_buffered_once(&mut buffered_futs);
                }

                // F-32: ストリーミングストリームを駆動。バックエンドタスクが生成したレスポンス
                // 断片を send_body、recv_body したリクエストボディをチャネルへ流す。
                if handler.h3_conn.is_some() {
                    did_work |= handler.drive_proxy_streams();
                }

                // タイマー再登録（コネクション処理直後）。
                schedule_timer(handler, &cid, &mut timers);

                if did_work {
                    // まだ仕事が残っている可能性 → dirty のまま維持して再投入する。
                    // `cid.clone()` が必要な理由: 同じ cid を dirty_queue（次回のダーティ処理
                    // 対象）と send_targets（今回の送出対象）の両方へ積む必要があるため
                    // （元のオブジェクトはどちらか一方にしか move できない）。`cid` は
                    // `ConnKey`（`Rc`）なので `clone()` は参照カウント +1 のみで malloc なし。
                    dirty_queue.push_back(cid.clone());
                } else {
                    handler.dirty = false;
                }
                send_targets.push(cid);
            }
        }

        // 送信処理（ダーティ接続だけを走査する、F-151）。sendmmsg / io_uring SENDMSG の
        // バッチ構築ロジック（`finalize_send_entry`/`send_mmsg_flush`/GSO セグメント判定）は
        // 変更しない。走査対象の集合を全接続からダーティ接続へ絞るだけ。
        //
        // **ACK 送出漏れが無いことの論拠**（レビュー確認事項）: `send_targets` は
        // 「このイテレーションでダーティ処理ループを通過した接続」の全量である
        // （did_work の有無に関わらず、ループで pop した cid は必ず `send_targets.push`
        // される）。ACK が必要になる契機は (1) 受信（`process_datagram_segments` が
        // recv 直後に必ずダーティ化する）(2) タイマー発火（`expire_due_timers` が
        // `on_timeout` 後に必ずダーティ化する）(3) バックエンド起床（`drain_wake_queue`
        // がダーティ化する）のいずれかであり、すべてダーティ化 → 今回のダーティ処理
        // ループを通過 → `send_targets` に入る、という経路を必ず通る。よってダーティで
        // ない（＝今回何も起きていない）接続にだけ ACK 送出漏れが起き得ないことになる。
        send_pending_packets(
            &connections,
            &socket,
            local_addr,
            mmsg_batch,
            &send_targets,
            &mut dirty_queue,
        )
        .await;

        // F-44: 協調的 yield。パケットが連続して到着すると select の recv arm が
        // 即 Ready になり続け、本タスクが単一 poll 内でループし続けて同一スレッドの
        // バックエンド I/O タスク（TLS ハンドシェイク・TCP 転送）が飢餓する。
        // 毎イテレーション一度キュー末尾へ譲り、spawn 済みタスクを 1 巡実行させる。
        crate::runtime::yield_now().await;
    }
}
