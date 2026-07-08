//! # HTTP/3 サーバー (monoio + quiche ベース)
//!
//! monoio (io_uring) と Cloudflare quiche を使用した HTTP/3 サーバー実装。
//! thread-per-core モデルで、各コネクションを独立した非同期タスクで処理します。
//!
//! ## 設計ポイント
//!
//! - **io_uring 活用**: monoio の UdpSocket で高効率な UDP I/O
//! - **コネクションごとのタスク分離**: monoio::spawn で各接続を独立管理
//! - **タイマー管理**: quiche::timeout() と monoio::time::sleep の連携
//! - **H3 インスタンスの永続化**: QPACK 動的テーブル等の状態を維持
//!
//! ## 機能
//!
//! - HTTP/1.1と同等のルーティング機能（ホスト/パスベース）
//! - セキュリティ機能（IP制限、レートリミット、メソッド制限）
//! - プロキシ機能（HTTPSバックエンドへのプロトコル変換）
//! - ファイル配信、リダイレクト、メトリクス

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::io::{self, Seek, Write as IoWrite};
use std::net::SocketAddr;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::udp::QuicUdpSocket;
use aws_lc_rs::rand::{SecureRandom, SystemRandom};
use bytes::{BufMut, Bytes, BytesMut};
use quiche::h3::NameValue;
use quiche::{h3, Config, ConnectionId};

/// F-32: ストリーミングのリクエストボディ recv_body 1 回分。
const REQ_RECV_CHUNK: usize = 16 * 1024;
/// F-32: リクエストボディチャネルの容量（アイテム数。バックプレッシャ）。
const REQ_CHAN_CAP: usize = 8;
/// F-32: レスポンス断片チャネルの容量（アイテム数。バックプレッシャ）。
const RESP_CHAN_CAP: usize = 8;

use ftlog::{debug, error, info, warn};

use crate::config::{
    resolve_http3_compression_config, AcceptedEncoding, Backend, CompressionConfig, ProxyTarget,
    SecurityConfig, UpstreamGroup, CURRENT_CONFIG, SHUTDOWN_FLAG,
};
use crate::logging::log_access;
use crate::metrics::encode_prometheus_metrics;
use crate::proxy::{check_security, SecurityCheckResult};
use crate::upstream::find_backend_unified;

/// memfd_create システムコールのラッパー（セキュリティ強化版）
///
/// 匿名のメモリファイルを作成します。このファイルはファイルシステム上には
/// 存在せず、メモリ上にのみ存在します。Landlock のファイルシステム制限を
/// バイパスしながら、ファイルディスクリプタ経由でアクセスできます。
///
/// ## セキュリティ対策
/// - MFD_CLOEXEC: exec() 時に自動的に閉じる（fd リーク防止）
/// - MFD_ALLOW_SEALING: 書き込み後にシールを適用可能にする
fn memfd_create_secure(name: &str) -> io::Result<std::fs::File> {
    let c_name = CString::new(name).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid memfd name: {}", e),
        )
    })?;

    // MFD_CLOEXEC (1): exec() 時に自動クローズ
    // MFD_ALLOW_SEALING (2): シール機能を有効化
    const MFD_CLOEXEC: libc::c_uint = 1;
    const MFD_ALLOW_SEALING: libc::c_uint = 2;

    let fd = unsafe { libc::memfd_create(c_name.as_ptr(), MFD_CLOEXEC | MFD_ALLOW_SEALING) };

    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// memfd にシールを適用（書き込み禁止・サイズ変更禁止）
///
/// シールを適用することで、memfd の内容が改ざんされることを防ぎます。
/// これにより、攻撃者が memfd の内容を書き換えて不正な証明書を
/// 注入することを防止できます。
fn apply_memfd_seals(fd: i32) -> io::Result<()> {
    // F_ADD_SEALS = 1033
    // F_SEAL_SEAL = 1 (これ以上シールを追加できなくする)
    // F_SEAL_SHRINK = 2 (サイズ縮小禁止)
    // F_SEAL_GROW = 4 (サイズ拡大禁止)
    // F_SEAL_WRITE = 8 (書き込み禁止)
    const F_ADD_SEALS: libc::c_int = 1033;
    const F_SEAL_SEAL: libc::c_int = 1;
    const F_SEAL_SHRINK: libc::c_int = 2;
    const F_SEAL_GROW: libc::c_int = 4;
    const F_SEAL_WRITE: libc::c_int = 8;

    let seals = F_SEAL_WRITE | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL;

    let result = unsafe { libc::fcntl(fd, F_ADD_SEALS, seals) };

    if result < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

/// PEM データを memfd に書き込み、/proc/self/fd/<fd> パスを返す（セキュリティ強化版）
///
/// この関数は以下のことを行います：
/// 1. memfd_create で匿名ファイルを作成（MFD_CLOEXEC + MFD_ALLOW_SEALING）
/// 2. PEM データを書き込み
/// 3. シールを適用（書き込み禁止・サイズ変更禁止・追加シール禁止）
/// 4. ファイル位置を先頭に戻す
/// 5. /proc/self/fd/<fd> パスを生成
///
/// ## セキュリティ特性
/// - memfd の内容は書き込み後に変更不可能（シール適用）
/// - exec() 時に自動的に閉じる（MFD_CLOEXEC）
/// - ファイルシステム上には存在しない（Landlock バイパス）
///
/// ## 注意
/// 戻り値の File オブジェクトはスコープ内で保持し続ける必要があります。
/// ドロップされると fd が閉じられ、パスが無効になります。
fn create_memfd_for_pem(name: &str, pem_data: &[u8]) -> io::Result<(std::fs::File, String)> {
    // memfd を作成（セキュリティフラグ付き）
    let mut memfd = memfd_create_secure(name)?;

    // PEM データを書き込み
    memfd.write_all(pem_data)?;

    // ファイル位置を先頭に戻す（読み取り用）
    memfd.seek(io::SeekFrom::Start(0))?;

    // /proc/self/fd/<fd> パスを生成
    let fd = memfd.as_raw_fd();
    let proc_path = format!("/proc/self/fd/{}", fd);

    // シールを適用（書き込み禁止、サイズ変更禁止）
    // 注意: シール適用後は quiche がファイルを読み取る必要があるため、
    // 読み取りは引き続き可能
    if let Err(e) = apply_memfd_seals(fd) {
        warn!(
            "[HTTP/3] Failed to apply memfd seals: {} (continuing without seals)",
            e
        );
        // シール適用失敗は致命的ではないため、警告のみで続行
    } else {
        debug!("[HTTP/3] memfd seals applied: WRITE|SHRINK|GROW|SEAL");
    }

    Ok((memfd, proc_path))
}

/// セキュアなバイト配列のゼロ化
///
/// メモリ上の機密データを安全にゼロ化します。
/// コンパイラによる最適化（デッドストア削除）を防ぐため、
/// volatile 書き込みを使用します。
fn secure_zero(data: &mut [u8]) {
    // volatile 書き込みで最適化を防止
    for byte in data.iter_mut() {
        unsafe {
            std::ptr::write_volatile(byte, 0);
        }
    }
    // メモリバリアで確実に書き込みを完了
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
}

/// HTTP/3 サーバー設定
#[derive(Clone)]
pub struct Http3ServerConfig {
    /// TLS 証明書パス（後方互換性のため残す、cert_pem優先）
    pub cert_path: String,
    /// TLS 秘密鍵パス（後方互換性のため残す、key_pem優先）
    pub key_path: String,
    /// TLS 証明書（PEM形式、事前読み込み済み）
    ///
    /// Landlock適用前に読み込まれた証明書バイト列。
    /// 設定されている場合、cert_pathより優先される。
    ///
    /// 注意: 使用後にセキュアにゼロ化されます。
    pub cert_pem: Option<Vec<u8>>,
    /// TLS 秘密鍵（PEM形式、事前読み込み済み）
    ///
    /// Landlock適用前に読み込まれた秘密鍵バイト列。
    /// 設定されている場合、key_pathより優先される。
    ///
    /// 注意: 使用後にセキュアにゼロ化されます。
    pub key_pem: Option<Vec<u8>>,
    /// 最大アイドルタイムアウト（ミリ秒）
    pub max_idle_timeout: u64,
    /// 最大 UDP ペイロードサイズ
    pub max_udp_payload_size: u64,
    /// 初期最大データサイズ
    pub initial_max_data: u64,
    /// 初期最大ストリームデータサイズ（双方向）
    pub initial_max_stream_data_bidi_local: u64,
    /// 初期最大ストリームデータサイズ（双方向リモート）
    pub initial_max_stream_data_bidi_remote: u64,
    /// 初期最大ストリームデータサイズ（単方向）
    pub initial_max_stream_data_uni: u64,
    /// 初期最大双方向ストリーム数
    pub initial_max_streams_bidi: u64,
    /// 初期最大単方向ストリーム数
    pub initial_max_streams_uni: u64,
    /// GSO/GRO を有効化するかどうか（デフォルト: false）
    pub gso_gro_enabled: bool,
}

impl Default for Http3ServerConfig {
    fn default() -> Self {
        Self {
            cert_path: String::new(),
            key_path: String::new(),
            cert_pem: None,
            key_pem: None,
            max_idle_timeout: 30000,
            max_udp_payload_size: 1350,
            initial_max_data: 10_000_000,
            initial_max_stream_data_bidi_local: 1_000_000,
            initial_max_stream_data_bidi_remote: 1_000_000,
            initial_max_stream_data_uni: 1_000_000,
            initial_max_streams_bidi: 100,
            initial_max_streams_uni: 100,
            gso_gro_enabled: false,
        }
    }
}

/// リクエスト 1 件のストリーミング状態（メインループ側、F-32）。
///
/// バックエンドタスクとはチャネル経由で接続される。本構造体はメインループのみが
/// 触り、`drive_proxy_stream` がフロー制御に従って `send_response`/`send_body`/`recv_body`
/// を駆動する。
struct ProxyStream {
    // ---- レスポンス方向（バックエンドタスク → メインループ） ----
    /// レスポンス断片の受信端。
    resp_rx: crate::http3_stream::Receiver<crate::http3_stream::RespMsg>,
    /// `send_response`（head）送出済みか。
    resp_started: bool,
    /// StreamBlocked で送出保留中の head（次回 drive で再送）。
    head_pending: Option<(u16, crate::http3_stream::RespHeaders)>,
    /// フロー制御で部分送信になったボディ断片（`(buf, 送信済みオフセット)`）。
    body_pending: Option<(Bytes, usize)>,
    /// レスポンス終端（EOF）を受信し、fin 送出が必要（フロー制御で保留中）。
    need_fin: bool,
    /// レスポンス fin 送出済み（= レスポンス完了）。
    resp_fin_sent: bool,

    // ---- リクエスト方向（メインループ → バックエンドタスク） ----
    /// リクエストボディ断片の送信端（クライアント END_STREAM で None にして EOF 伝播）。
    req_tx: Option<crate::http3_stream::Sender<Bytes>>,
    /// チャネルへ未投入のボディ（初回バッチ分／満杯時の溢れ）。
    req_pending: VecDeque<Bytes>,
    /// quiche にリクエストボディの読み取り可能データがある（Data イベントで true）。
    req_readable: bool,
    /// クライアント END_STREAM（Finished）受信済み。
    req_eof_seen: bool,
    /// 受信済みリクエストボディ累計（`max_request_body_size` 強制用）。
    req_bytes_total: u64,
    /// 許容リクエストボディ上限（0 = 無制限）。
    max_request_body: u64,
    /// ボディ上限超過（413 + ストリームリセット）。
    req_too_large: bool,
}

impl ProxyStream {
    fn new(
        resp_rx: crate::http3_stream::Receiver<crate::http3_stream::RespMsg>,
        req_tx: crate::http3_stream::Sender<Bytes>,
        has_body: bool,
        max_request_body: u64,
    ) -> Self {
        Self {
            resp_rx,
            resp_started: false,
            head_pending: None,
            body_pending: None,
            need_fin: false,
            resp_fin_sent: false,
            req_tx: Some(req_tx),
            req_pending: VecDeque::new(),
            req_readable: false,
            req_eof_seen: !has_body,
            req_bytes_total: 0,
            max_request_body,
            req_too_large: false,
        }
    }
}

/// バッファリング（非ストリーミング）経路の保留リクエスト（F-32）。
///
/// ストリーミング非適格なリクエストは END_STREAM 受信まで `stream_bodies` にボディを
/// 蓄積し、完了時に既存の `handle_request`（バッファ経路）で処理する。
struct BufferedReq {
    /// リクエストヘッダ（所有）。
    headers: Vec<h3::Header>,
    /// END_STREAM 受信済み（= 処理可能）。
    end: bool,
}

/// メインループの `select_biased!` 受信結果（F-32）。
enum RecvOutcome {
    /// UDP パケット受信（GRO 集約結果）。
    Packet(io::Result<crate::udp::socket::GroRecvResult>),
    /// バックエンドタスクからの起床通知。
    Notified,
    /// タイムアウトティック。
    Timeout,
}

/// `classify` の判定結果。
///
/// `classify` の戻り値として生成後に即座に `match` される一時スタック値であり、コレクション
/// へ格納しない。`Stream` の中身を `Box` 化するとリクエストごとにヒープ確保が増えゼロ
/// アロケーション原則に反するため、サイズ差は許容する（large_enum_variant を allow）。
#[allow(clippy::large_enum_variant)]
enum Decision {
    /// ストリーミング適格 → バックエンドタスクを spawn。
    Stream(crate::http3_stream::BackendTaskParams),
    /// 非適格 → バッファ経路（`handle_request`）。
    Buffer,
    /// classify が即時応答済み（セキュリティ拒否など）。
    Handled,
}

/// HTTP/3 コネクションハンドラー
///
/// quiche::Connection と h3::Connection をセットで保持し、
/// コネクションの寿命の間、同一のインスタンスを維持します。
///
/// HTTP/1.1と同等のルーティング・セキュリティ・プロキシ機能をサポート。
struct Http3Handler {
    /// QUIC コネクション
    conn: quiche::Connection,
    /// HTTP/3 コネクション (確立後に Some)
    h3_conn: Option<h3::Connection>,
    /// リモートアドレス
    peer_addr: SocketAddr,
    /// 部分的なレスポンス（ストリーム ID → (ボディ, 書き込み済みバイト数)）
    partial_responses: HashMap<u64, (Vec<u8>, usize)>,
    /// クライアントIPアドレス（文字列）
    client_ip: String,
    /// ストリーミングプロキシ中のストリーム（F-32）。
    proxy_streams: HashMap<u64, ProxyStream>,
    /// バッファ経路の保留リクエスト（F-32）。
    buffered_reqs: HashMap<u64, BufferedReq>,
    /// ストリームごとのリクエストボディ蓄積（バッファ経路 + ストリーミング初回バッチ）。
    stream_bodies: HashMap<u64, BytesMut>,
    /// バックエンドタスク → メインループの起床通知（F-32）。
    notify: crate::http3_stream::H3Notify,
    /// バックエンドタスクのスポーナ（F-46: 型付きタスクプール。ワーカースレッドで共有）。
    backend_spawner: crate::http3_stream::BackendSpawner,
}

impl Http3Handler {
    /// 新しいハンドラーを作成
    fn new(
        conn: quiche::Connection,
        peer_addr: SocketAddr,
        notify: crate::http3_stream::H3Notify,
        backend_spawner: crate::http3_stream::BackendSpawner,
    ) -> Self {
        Self {
            conn,
            h3_conn: None,
            client_ip: peer_addr.ip().to_string(),
            peer_addr,
            partial_responses: HashMap::new(),
            proxy_streams: HashMap::new(),
            buffered_reqs: HashMap::new(),
            stream_bodies: HashMap::new(),
            notify,
            backend_spawner,
        }
    }

    /// HTTP/3 コネクションを初期化（QUIC 確立後）
    fn init_h3(&mut self) -> io::Result<()> {
        if self.h3_conn.is_none() && self.conn.is_established() {
            let h3_config = h3::Config::new().map_err(|e| io::Error::other(e.to_string()))?;
            let h3 = h3::Connection::with_transport(&mut self.conn, &h3_config)
                .map_err(|e| io::Error::other(e.to_string()))?;
            self.h3_conn = Some(h3);
            debug!(
                "[HTTP/3] HTTP/3 connection established from {}",
                self.peer_addr
            );
        }
        Ok(())
    }

    /// HTTP/3 イベントを処理（F-32: ストリーミング/バッファ分岐）
    ///
    /// poll で全イベントを収集（Headers 列挙・Data 排出・Finished 記録）した後、
    /// 各 Headers を `classify` で **ストリーミング適格／バッファ／即時応答済み** に振り分ける。
    /// ストリーミング適格はバックエンドタスクを spawn し `proxy_streams` に登録、非適格は
    /// END_STREAM 受信後に既存 `handle_request`（バッファ経路）で処理する。
    ///
    /// Data 排出は、**既にストリーミング中のストリーム**には `req_readable` を立てるだけで
    /// `recv_body` せず（バックプレッシャ対応の `drive_proxy_stream` に委譲）、それ以外は
    /// `stream_bodies` へ蓄積する（バッファ経路 + ストリーミング初回バッチ）。
    async fn process_h3_events(&mut self) -> io::Result<()> {
        // 新規 Headers（stream_id, headers, more_frames）と Finished / Reset を収集。
        let mut new_headers: Vec<(u64, Vec<h3::Header>, bool)> = Vec::new();
        let mut finished: Vec<u64> = Vec::new();
        let mut reset: Vec<u64> = Vec::new();

        if let Some(ref mut h3_conn) = self.h3_conn {
            loop {
                match h3_conn.poll(&mut self.conn) {
                    Ok((stream_id, h3::Event::Headers { list, more_frames })) => {
                        debug!(
                            "[HTTP/3] Headers: stream_id={}, more_frames={}, headers={}",
                            stream_id,
                            more_frames,
                            list.len()
                        );
                        new_headers.push((stream_id, list, more_frames));
                    }
                    Ok((stream_id, h3::Event::Data)) => {
                        if let Some(ps) = self.proxy_streams.get_mut(&stream_id) {
                            // ストリーミング中: バックプレッシャ対応の pump に委譲。
                            ps.req_readable = true;
                        } else {
                            // バッファ経路（または未分類の初回バッチ）: stream_bodies へ排出。
                            let body = self.stream_bodies.entry(stream_id).or_default();
                            loop {
                                body.reserve(REQ_RECV_CHUNK);
                                let spare = body.spare_capacity_mut();
                                // SAFETY: recv_body は書き込み専用で read バイトのみ初期化する。
                                // spare は BytesMut の確保済み有効領域。advance_mut で len に反映。
                                let spare_u8 = unsafe {
                                    std::slice::from_raw_parts_mut(
                                        spare.as_mut_ptr() as *mut u8,
                                        spare.len(),
                                    )
                                };
                                match h3_conn.recv_body(&mut self.conn, stream_id, spare_u8) {
                                    Ok(read) if read > 0 => unsafe { body.advance_mut(read) },
                                    Ok(_) => break,
                                    Err(h3::Error::Done) => break,
                                    Err(e) => {
                                        warn!("[HTTP/3] recv_body error: {}", e);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Ok((stream_id, h3::Event::Finished)) => finished.push(stream_id),
                    Ok((stream_id, h3::Event::Reset(_))) => reset.push(stream_id),
                    Ok((_flow_id, h3::Event::GoAway)) => {}
                    Ok((_, h3::Event::PriorityUpdate)) => {}
                    Err(h3::Error::Done) => break,
                    Err(e) => {
                        warn!("[HTTP/3] h3 poll error: {}", e);
                        break;
                    }
                }
            }
        }

        // --- 新規 Headers を分類して振り分け ---
        for (stream_id, headers, more_frames) in new_headers {
            match self.classify(stream_id, &headers, more_frames) {
                Decision::Stream(params) => {
                    let (req_tx, req_rx) = crate::http3_stream::channel::<Bytes>(REQ_CHAN_CAP);
                    let (resp_tx, resp_rx) =
                        crate::http3_stream::channel::<crate::http3_stream::RespMsg>(RESP_CHAN_CAP);
                    let mut ps =
                        ProxyStream::new(resp_rx, req_tx, more_frames, params.max_request_body);
                    // 初回バッチで届いていたボディを取り込む。
                    if let Some(body) = self.stream_bodies.remove(&stream_id) {
                        if !body.is_empty() {
                            ps.req_bytes_total += body.len() as u64;
                            ps.req_pending.push_back(body.freeze());
                        }
                    }
                    (self.backend_spawner)(params, req_rx, resp_tx, self.notify.clone());
                    self.proxy_streams.insert(stream_id, ps);
                }
                Decision::Buffer => {
                    self.buffered_reqs.insert(
                        stream_id,
                        BufferedReq {
                            headers,
                            end: !more_frames,
                        },
                    );
                }
                Decision::Handled => {
                    self.stream_bodies.remove(&stream_id);
                }
            }
        }

        // --- Finished / Reset 反映 ---
        for stream_id in finished {
            if let Some(ps) = self.proxy_streams.get_mut(&stream_id) {
                ps.req_eof_seen = true;
            } else if let Some(br) = self.buffered_reqs.get_mut(&stream_id) {
                br.end = true;
            }
        }
        for stream_id in reset {
            // ストリームを破棄（チャネル drop でバックエンドタスクも中断）。
            self.proxy_streams.remove(&stream_id);
            self.buffered_reqs.remove(&stream_id);
            self.stream_bodies.remove(&stream_id);
        }

        // --- 完了したバッファ経路リクエストを処理 ---
        let ready: Vec<u64> = self
            .buffered_reqs
            .iter()
            .filter(|(_, b)| b.end)
            .map(|(k, _)| *k)
            .collect();
        for stream_id in ready {
            let br = self.buffered_reqs.remove(&stream_id).unwrap();
            let body = self
                .stream_bodies
                .remove(&stream_id)
                .map(|b| b.to_vec())
                .unwrap_or_default();
            self.handle_request(stream_id, &br.headers, &body).await?;
        }

        // 部分的なレスポンスを送信（非ストリーミング経路）。
        self.flush_partial_responses()?;

        // ストリーミング駆動はメインループの毎イテレーション drive で行う（通知/タイムアウト時も
        // 確実に進めるため。ここで重複呼び出ししない）。

        Ok(())
    }

    /// すべてのストリーミングストリームを 1 回駆動する（req pump + resp flush）。
    ///
    /// メインループから毎イテレーション呼ばれ、フロー制御に従って `recv_body`→req チャネル、
    /// resp チャネル→`send_response`/`send_body` を進める。完了したストリームは除去する。
    fn drive_proxy_streams(&mut self) {
        let h3 = match self.h3_conn.as_mut() {
            Some(h) => h,
            None => return,
        };
        let conn = &mut self.conn;
        let mut done: Vec<u64> = Vec::new();
        for (&stream_id, ps) in self.proxy_streams.iter_mut() {
            if drive_proxy_stream(h3, conn, stream_id, ps) {
                done.push(stream_id);
            }
        }
        for stream_id in done {
            debug!("[HTTP/3] streaming proxy stream {} done", stream_id);
            self.proxy_streams.remove(&stream_id);
        }
    }

    /// リクエストをストリーミング適格・バッファ・即時応答済みに分類する（F-32）。
    ///
    /// ストリーミング適格条件: **Proxy バックエンド + バッファリング非 Full + 非 gRPC +
    /// WASM モジュール非適用 + 平文バックエンド（TLS 以外）+ セキュリティ許可**。
    /// セキュリティ拒否は（大容量アップロードを溜め込まないよう）**即時に拒否応答**して
    /// `Handled` を返す。それ以外の非適格（メトリクス・非 Proxy・404・gRPC・full・wasm・
    /// TLS・サーバ選択失敗）は `Buffer` を返し、既存の `handle_request` が処理する。
    fn classify(&mut self, stream_id: u64, headers: &[h3::Header], more_frames: bool) -> Decision {
        // --- 疑似ヘッダ + 必要ヘッダを抽出 ---
        let mut method: Option<&[u8]> = None;
        let mut path: Option<&[u8]> = None;
        let mut authority: &[u8] = b"";
        let mut content_length: usize = 0;
        let mut accept_encoding: Option<&[u8]> = None;
        let mut user_agent: &[u8] = b"";
        for h in headers {
            let name = h.name();
            if name == b":method" {
                method = Some(h.value());
            } else if name == b":path" {
                path = Some(h.value());
            } else if name == b":authority" {
                authority = h.value();
            } else if name.eq_ignore_ascii_case(b"content-length") {
                if let Ok(s) = std::str::from_utf8(h.value()) {
                    content_length = s.trim().parse().unwrap_or(0);
                }
            } else if name.eq_ignore_ascii_case(b"accept-encoding") {
                accept_encoding = Some(h.value());
            } else if name.eq_ignore_ascii_case(b"user-agent") {
                user_agent = h.value();
            }
        }
        let method = method.unwrap_or(b"GET");
        let path = path.unwrap_or(b"/");

        let config = CURRENT_CONFIG.load();

        // メトリクスエンドポイントはバッファ経路（handle_request が配信、GET・ボディなし）。
        {
            let prom = &config.prometheus_config;
            if prom.enabled {
                if let Ok(p) = std::str::from_utf8(path) {
                    let p2 = p.split('?').next().unwrap_or(p);
                    if p2 == prom.path {
                        return Decision::Buffer;
                    }
                }
            }
        }

        // --- ルーティング ---
        let headers_raw: Vec<(&[u8], &[u8])> = headers
            .iter()
            .filter(|h| !h.name().starts_with(b":"))
            .map(|h| (h.name(), h.value()))
            .collect();
        let query_start = path.iter().position(|&b| b == b'?');
        let raw_query: &[u8] = query_start.map(|i| &path[i + 1..]).unwrap_or(b"");
        let path_wo_query = query_start.map(|i| &path[..i]).unwrap_or(path);

        let backend_result = find_backend_unified(
            authority,
            path_wo_query,
            method,
            &headers_raw,
            raw_query,
            &self.peer_addr,
            config.route.as_slice(),
            &config.upstream_groups,
        )
        .or_else(|| {
            if !authority.is_empty() {
                find_backend_unified(
                    b"",
                    path_wo_query,
                    method,
                    &headers_raw,
                    raw_query,
                    &self.peer_addr,
                    config.route.as_slice(),
                    &config.upstream_groups,
                )
            } else {
                None
            }
        });

        let (prefix, backend, _route_compression) = match backend_result {
            Some(b) => b,
            None => return Decision::Buffer, // handle_request -> 404（gRPC 含む）
        };

        // Proxy バックエンドのみストリーミング対象。
        let (upstream_group, path_compression, buffering, _modules) = match &backend {
            Backend::Proxy(ug, _sec, comp, buf, _cache, mods) => {
                (ug.clone(), comp.clone(), buf.clone(), mods.clone())
            }
            _ => return Decision::Buffer,
        };

        // バッファリング full は全バッファ経路。
        if buffering.mode == crate::buffering::BufferingMode::Full {
            return Decision::Buffer;
        }
        // WASM モジュール適用ありはボディ全体が必要 → バッファ経路。
        #[cfg(feature = "wasm")]
        if _modules.as_deref().map(|m| !m.is_empty()).unwrap_or(false) {
            return Decision::Buffer;
        }
        // gRPC（トレーラー）はバッファ経路。
        #[cfg(feature = "grpc")]
        if Self::is_grpc_request(headers) {
            return Decision::Buffer;
        }

        // セキュリティチェック（ストリーミング適格は早期拒否でアップロードを溜めない）。
        let security = backend.security();
        let check = check_security(security, &self.client_ip, method, content_length, false);
        if check != SecurityCheckResult::Allowed {
            let status = check.status_code();
            let msg = check.message();
            let _ = self.send_error_response(stream_id, status, msg);
            log_access(
                method,
                authority,
                path,
                user_agent,
                content_length as u64,
                status,
                msg.len() as u64,
                Instant::now(),
                &self.client_ip,
                "",
            );
            return Decision::Handled;
        }

        // サーバ選択。
        let server = match upstream_group.select(&self.client_ip) {
            Some(s) => s.clone(),
            None => return Decision::Buffer, // handle_request -> 502
        };

        // --- リクエスト head 構築 ---
        let client_encoding = accept_encoding
            .map(AcceptedEncoding::parse)
            .unwrap_or(AcceptedEncoding::Identity);
        let compression = resolve_http3_compression_config(&path_compression, &config.http3_config);
        let final_path = compute_backend_path(&server.target, path, &prefix);
        let request_head = build_h1_request_head(&server.target, method, &final_path, headers);

        // F-44: TLS バックエンドもストリーミング対象（バックエンドタスクが全二重 TLS で貫通）。
        let use_tls = server.target.use_tls;
        let sni = server.target.sni().to_string();
        let tls_insecure = upstream_group.tls_insecure();

        Decision::Stream(crate::http3_stream::BackendTaskParams {
            server,
            request_head,
            has_request_body: more_frames,
            compression,
            client_encoding,
            timeout_secs: 30,
            max_request_body: security.max_request_body_size as u64,
            use_tls,
            sni,
            tls_insecure,
        })
    }

    /// HTTP/3 リクエストを処理（完全版）
    ///
    /// HTTP/1.1と同等のルーティング・セキュリティ・プロキシ機能をサポート。
    async fn handle_request(
        &mut self,
        stream_id: u64,
        headers: &[h3::Header],
        request_body: &[u8],
    ) -> io::Result<()> {
        // HTTP/3コネクションが確立されていなければ何もしない
        if self.h3_conn.is_none() {
            return Ok(());
        }

        // ヘッダーを解析
        let mut method = None;
        let mut path = None;
        let mut authority = None;
        let mut content_length: usize = 0;
        let mut accept_encoding: Option<Vec<u8>> = None;
        let mut user_agent: Vec<u8> = Vec::new();

        for header in headers {
            match header.name() {
                b":method" => method = Some(header.value().to_vec()),
                b":path" => path = Some(header.value().to_vec()),
                b":authority" => authority = Some(header.value().to_vec()),
                b"content-length" => {
                    if let Ok(s) = std::str::from_utf8(header.value()) {
                        content_length = s.parse().unwrap_or(0);
                    }
                }
                name if name.eq_ignore_ascii_case(b"accept-encoding") => {
                    accept_encoding = Some(header.value().to_vec());
                }
                name if name.eq_ignore_ascii_case(b"user-agent") => {
                    user_agent = header.value().to_vec();
                }
                _ => {}
            }
        }

        // クライアントの Accept-Encoding を解析
        let client_encoding = accept_encoding
            .as_ref()
            .map(|v| AcceptedEncoding::parse(v))
            .unwrap_or(AcceptedEncoding::Identity);

        let method = method.unwrap_or_else(|| b"GET".to_vec());
        let path = path.unwrap_or_else(|| b"/".to_vec());
        let authority = authority.unwrap_or_default();

        // 処理開始時刻
        let start_time = Instant::now();

        debug!(
            "[HTTP/3] Request: {} {} (stream {})",
            String::from_utf8_lossy(&method),
            String::from_utf8_lossy(&path),
            stream_id
        );

        // gRPC リクエスト検出フラグ
        #[cfg(feature = "grpc")]
        let is_grpc = Self::is_grpc_request(headers);
        #[cfg(not(feature = "grpc"))]
        let _is_grpc = false;

        #[cfg(feature = "grpc")]
        if is_grpc {
            debug!(
                "[HTTP/3] gRPC request detected: {}",
                String::from_utf8_lossy(&path)
            );
        }

        // メトリクスエンドポイント（設定可能なパス）
        {
            let config = CURRENT_CONFIG.load();
            let prom_config = &config.prometheus_config;

            let path_str = std::str::from_utf8(&path).unwrap_or("/");
            if prom_config.enabled && path_str == prom_config.path && method == b"GET" {
                // IPアドレス制限チェック
                if !prom_config.is_ip_allowed(&self.client_ip) {
                    self.send_error_response(stream_id, 403, b"Forbidden")?;
                    let user_agent_slice: &[u8] = if user_agent.is_empty() {
                        &[]
                    } else {
                        &user_agent
                    };
                    log_access(
                        &method,
                        &authority,
                        &path,
                        user_agent_slice,
                        request_body.len() as u64,
                        403,
                        9,
                        start_time,
                        &self.client_ip,
                        "",
                    );
                    return Ok(());
                }

                let body = encode_prometheus_metrics();
                self.send_response(
                    stream_id,
                    200,
                    &[
                        (b":status", b"200"),
                        (b"content-type", b"text/plain; version=0.0.4; charset=utf-8"),
                        (b"server", b"veil/http3"),
                    ],
                    Some(&body),
                )?;

                let user_agent_slice: &[u8] = if user_agent.is_empty() {
                    &[]
                } else {
                    &user_agent
                };
                log_access(
                    &method,
                    &authority,
                    &path,
                    user_agent_slice,
                    request_body.len() as u64,
                    200,
                    body.len() as u64,
                    start_time,
                    &self.client_ip,
                    "",
                );
                return Ok(());
            }
        }

        // バックエンド選択（統合ルーティング）
        let config = CURRENT_CONFIG.load();

        // ヘッダーをゼロコピーのバイト列スライスとして参照（HashMap 不要）
        let headers_raw: Vec<(&[u8], &[u8])> = headers
            .iter()
            .filter(|h| !h.name().starts_with(b":")) // 疑似ヘッダーを除外
            .map(|h| (h.name(), h.value()))
            .collect();

        // パス/クエリ分離（スキャンを1回に統一）
        let query_start_pos = path.iter().position(|&b| b == b'?');
        let raw_query: &[u8] = query_start_pos.map(|i| &path[i + 1..]).unwrap_or(b"");
        let path_without_query = query_start_pos.map(|i| &path[..i]).unwrap_or(&path);

        let backend_result = find_backend_unified(
            &authority,
            path_without_query,
            &method,
            &headers_raw,
            raw_query,
            &self.peer_addr,
            config.route.as_slice(),
            &config.upstream_groups,
        )
        .or_else(|| {
            // authority が空でない場合、デフォルトルートを検索
            if !authority.is_empty() {
                debug!(
                    "[HTTP/3] No route found for authority '{}', trying default routes",
                    String::from_utf8_lossy(&authority)
                );
                find_backend_unified(
                    b"",
                    path_without_query,
                    &method,
                    &headers_raw,
                    raw_query,
                    &self.peer_addr,
                    config.route.as_slice(),
                    &config.upstream_groups,
                )
            } else {
                None
            }
        });

        let (prefix, backend, _route_compression) = match backend_result {
            Some(b) => b,
            None => {
                debug!(
                    "[HTTP/3] No backend found for authority='{}', path='{}'",
                    String::from_utf8_lossy(&authority),
                    String::from_utf8_lossy(&path)
                );

                // gRPC リクエストの場合は gRPC エラーレスポンスを返す
                #[cfg(feature = "grpc")]
                if is_grpc {
                    // UNIMPLEMENTED (12) - サービス/メソッドが見つからない
                    self.send_grpc_response(stream_id, &[], None, 12, Some("Service not found"))?;
                    let user_agent_slice: &[u8] = if user_agent.is_empty() {
                        &[]
                    } else {
                        &user_agent
                    };
                    log_access(
                        &method,
                        &authority,
                        &path,
                        user_agent_slice,
                        request_body.len() as u64,
                        200,
                        0,
                        start_time,
                        &self.client_ip,
                        "",
                    );
                    return Ok(());
                }

                self.send_error_response(stream_id, 404, b"Not Found")?;
                let user_agent_slice: &[u8] = if user_agent.is_empty() {
                    &[]
                } else {
                    &user_agent
                };
                log_access(
                    &method,
                    &authority,
                    &path,
                    user_agent_slice,
                    request_body.len() as u64,
                    404,
                    9,
                    start_time,
                    &self.client_ip,
                    "",
                );
                return Ok(());
            }
        };

        // セキュリティチェック
        let security = backend.security();
        let check_result =
            check_security(security, &self.client_ip, &method, content_length, false);

        if check_result != SecurityCheckResult::Allowed {
            let status = check_result.status_code();
            let msg = check_result.message();
            self.send_error_response(stream_id, status, msg)?;
            let user_agent_slice: &[u8] = if user_agent.is_empty() {
                &[]
            } else {
                &user_agent
            };
            log_access(
                &method,
                &authority,
                &path,
                user_agent_slice,
                request_body.len() as u64,
                status,
                msg.len() as u64,
                start_time,
                &self.client_ip,
                "",
            );
            return Ok(());
        }

        // WASMモジュールの適用
        #[cfg(feature = "wasm")]
        {
            let config = CURRENT_CONFIG.load();
            if let Some(ref wasm_engine) = config.wasm_filter_engine {
                let path_str = std::str::from_utf8(&path).unwrap_or("/");
                let method_str = std::str::from_utf8(&method).unwrap_or("GET");

                // F-43: モジュールリストは Arc 共有（リクエストごとの deep copy 排除）
                let modules_to_apply = if let Some(backend_modules) = backend.modules_arc() {
                    backend_modules.clone()
                } else {
                    // ルートレベルのmodulesが指定されていない場合は、WASMモジュールを適用しない
                    crate::wasm::empty_wasm_modules()
                };

                if !modules_to_apply.is_empty() {
                    // HTTP/3のヘッダーを取得
                    let headers_vec: Vec<(Vec<u8>, Vec<u8>)> = headers
                        .iter()
                        .filter(|h| !h.name().starts_with(b":")) // 疑似ヘッダーを除外
                        .map(|h| (h.name().to_vec(), h.value().to_vec()))
                        .collect();

                    let wasm_result = wasm_engine
                        .on_request_headers_with_modules(
                            &modules_to_apply,
                            &std::sync::Arc::from(path_str),
                            &std::sync::Arc::from(method_str),
                            headers_vec,
                            &std::sync::Arc::from(self.client_ip.as_str()),
                            request_body.is_empty(), // end_of_stream
                        )
                        .await;

                    match wasm_result {
                        crate::wasm::FilterResult::LocalResponse(resp) => {
                            // ローカルレスポンスを返送
                            self.send_response(
                                stream_id,
                                resp.status_code,
                                &resp
                                    .headers
                                    .iter()
                                    .map(|(k, v)| (k.as_slice(), v.as_slice()))
                                    .collect::<Vec<_>>(),
                                Some(&resp.body),
                            )?;
                            let user_agent_slice: &[u8] = if user_agent.is_empty() {
                                &[]
                            } else {
                                &user_agent
                            };
                            log_access(
                                &method,
                                &authority,
                                &path,
                                user_agent_slice,
                                request_body.len() as u64,
                                resp.status_code,
                                resp.body.len() as u64,
                                start_time,
                                &self.client_ip,
                                "",
                            );
                            return Ok(());
                        }
                        crate::wasm::FilterResult::Pause => {
                            warn!("WASM module requested pause, but async operations are not yet supported");
                        }
                        crate::wasm::FilterResult::Continue { .. } => {
                            // ヘッダー変更はHTTP/3では複雑なため、現時点ではスキップ
                            // 将来的に実装可能
                        }
                    }
                }
            }
        }

        // バックエンド処理
        let (status, resp_size) = match backend {
            Backend::Proxy(upstream_group, _, path_compression, _buffering, _cache, _) => {
                debug!("[HTTP/3] Starting proxy request to upstream group");

                // HTTP/3専用圧縮設定を解決
                // 優先順位: パス設定 > HTTP/3設定 > デフォルト
                let config = CURRENT_CONFIG.load();
                let effective_compression =
                    resolve_http3_compression_config(&path_compression, &config.http3_config);

                let result = self
                    .handle_proxy(
                        stream_id,
                        &upstream_group,
                        &effective_compression,
                        client_encoding,
                        &method,
                        &path,
                        &prefix,
                        headers,
                        request_body,
                    )
                    .await
                    .unwrap_or((502, 11));
                debug!(
                    "[HTTP/3] Proxy request completed: status={}, size={}",
                    result.0, result.1
                );
                result
            }
            Backend::MemoryFile(data, mime_type, security, _) => {
                // パス完全一致チェック
                let path_str = std::str::from_utf8(&path).unwrap_or("/");
                let prefix_str = std::str::from_utf8(&prefix).unwrap_or("");

                let remainder = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
                    &path_str[prefix_str.len()..]
                } else {
                    ""
                };

                let clean_remainder = remainder.trim_matches('/');
                if !clean_remainder.is_empty() {
                    self.send_error_response(stream_id, 404, b"Not Found")?;
                    (404, 9)
                } else {
                    let mut resp_headers: Vec<(&[u8], &[u8])> = vec![
                        (b"content-type", mime_type.as_bytes()),
                        (b"server", b"veil/http3"),
                    ];

                    // セキュリティヘッダー追加
                    let security_headers: Vec<(Vec<u8>, Vec<u8>)> = security
                        .add_response_headers
                        .iter()
                        .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
                        .collect();

                    for (k, v) in &security_headers {
                        resp_headers.push((k.as_slice(), v.as_slice()));
                    }

                    self.send_response(stream_id, 200, &resp_headers, Some(&data))?;
                    (200, data.len())
                }
            }
            Backend::SendFile(base_path, is_dir, index_file, security, _cache, _, _) => self
                .handle_sendfile(
                    stream_id,
                    &base_path,
                    is_dir,
                    index_file.as_deref(),
                    &path,
                    &prefix,
                    &security,
                )
                .await
                .unwrap_or((404, 9)),
            Backend::Redirect(redirect_url, status_code, preserve_path, _) => self
                .handle_redirect(
                    stream_id,
                    &redirect_url,
                    status_code,
                    preserve_path,
                    &path,
                    &prefix,
                )
                .unwrap_or((500, 0)),
        };

        let user_agent_slice: &[u8] = if user_agent.is_empty() {
            &[]
        } else {
            &user_agent
        };
        log_access(
            &method,
            &authority,
            &path,
            user_agent_slice,
            request_body.len() as u64,
            status,
            resp_size as u64,
            start_time,
            &self.client_ip,
            "",
        );
        Ok(())
    }

    /// レスポンス送信ヘルパー
    ///
    /// HTTP/3 レスポンスを送信します。StreamBlocked エラーが発生した場合は
    /// 部分レスポンスとして保存し、後で flush_partial_responses() で再送します。
    fn send_response(
        &mut self,
        stream_id: u64,
        status: u16,
        headers: &[(&[u8], &[u8])],
        body: Option<&[u8]>,
    ) -> io::Result<()> {
        debug!(
            "[HTTP/3] send_response called: stream_id={}, status={}, h3_conn={}",
            stream_id,
            status,
            self.h3_conn.is_some()
        );

        let h3_conn = match &mut self.h3_conn {
            Some(h3) => h3,
            None => {
                warn!("[HTTP/3] h3_conn is None, cannot send response");
                return Ok(());
            }
        };

        // ステータスを含むヘッダーを構築（itoa::Buffer使用でヒープ割り当て削減）
        let mut status_buf = itoa::Buffer::new();
        let status_str = status_buf.format(status);
        let mut h3_headers = vec![h3::Header::new(b":status", status_str.as_bytes())];

        for (name, value) in headers {
            if *name != b":status" {
                h3_headers.push(h3::Header::new(name, value));
            }
        }

        // Content-Length を追加（itoa::Buffer使用）
        if let Some(body_data) = body {
            let mut len_buf = itoa::Buffer::new();
            let len_str = len_buf.format(body_data.len());
            h3_headers.push(h3::Header::new(b"content-length", len_str.as_bytes()));
        }

        // ヘッダー送信
        let has_body = body.is_some() && body.is_some_and(|b| !b.is_empty());
        match h3_conn.send_response(&mut self.conn, stream_id, &h3_headers, !has_body) {
            Ok(()) => {
                debug!("[HTTP/3] Response headers sent for stream {}", stream_id);
            }
            Err(h3::Error::StreamBlocked) => {
                // ストリームがブロックされた場合、ボディを部分レスポンスとして保存
                // 次の send_pending_packets() で送信される
                debug!("[HTTP/3] Stream {} blocked, will retry later", stream_id);
                if let Some(body_data) = body {
                    self.partial_responses
                        .insert(stream_id, (body_data.to_vec(), 0));
                }
                return Ok(());
            }
            Err(e) => {
                warn!(
                    "[HTTP/3] send_response error on stream {}: {}",
                    stream_id, e
                );
                return Ok(());
            }
        }

        // ボディ送信
        if let Some(body_data) = body {
            if !body_data.is_empty() {
                match h3_conn.send_body(&mut self.conn, stream_id, body_data, true) {
                    Ok(written) => {
                        debug!(
                            "[HTTP/3] Response body sent: {} bytes for stream {}",
                            written, stream_id
                        );
                        // 部分的にしか送信できなかった場合
                        if written < body_data.len() {
                            self.partial_responses
                                .insert(stream_id, (body_data.to_vec(), written));
                        }
                    }
                    Err(h3::Error::Done) => {
                        // バッファがいっぱい、後で再送
                        debug!(
                            "[HTTP/3] Body buffer full for stream {}, queuing for later",
                            stream_id
                        );
                        self.partial_responses
                            .insert(stream_id, (body_data.to_vec(), 0));
                    }
                    Err(e) => {
                        warn!("[HTTP/3] send_body error on stream {}: {}", stream_id, e);
                    }
                }
            }
        }

        Ok(())
    }

    /// エラーレスポンス送信
    fn send_error_response(&mut self, stream_id: u64, status: u16, body: &[u8]) -> io::Result<()> {
        debug!(
            "[HTTP/3] Sending error response: status={}, body_len={}",
            status,
            body.len()
        );
        let result = self.send_response(
            stream_id,
            status,
            &[(b"content-type", b"text/plain"), (b"server", b"veil/http3")],
            Some(body),
        );
        debug!("[HTTP/3] Error response send result: {:?}", result.is_ok());
        result
    }

    /// gRPC リクエストかどうかを判定
    ///
    /// Content-Type ヘッダーが `application/grpc` で始まる場合にgRPCリクエストと判定。
    #[cfg(feature = "grpc")]
    fn is_grpc_request(headers: &[h3::Header]) -> bool {
        for header in headers {
            if header.name().eq_ignore_ascii_case(b"content-type") {
                return crate::grpc::headers::is_grpc_content_type(header.value());
            }
        }
        false
    }

    /// gRPC レスポンスを送信 (トレイラー付き)
    ///
    /// HTTP/3 では QPACK を使用してトレイラーを送信します。
    /// ボディ送信後に grpc-status と grpc-message をトレイラーとして送信。
    #[cfg(feature = "grpc")]
    fn send_grpc_response(
        &mut self,
        stream_id: u64,
        headers: &[(&[u8], &[u8])],
        body: Option<&[u8]>,
        grpc_status: u32,
        grpc_message: Option<&str>,
    ) -> io::Result<()> {
        let h3_conn = match &mut self.h3_conn {
            Some(h3) => h3,
            None => return Ok(()),
        };

        // 1. ヘッダー送信 (200 OK + content-type: application/grpc)
        let mut h3_headers = vec![
            h3::Header::new(b":status", b"200"),
            h3::Header::new(b"content-type", b"application/grpc+proto"),
        ];

        for &(name, value) in headers {
            if name != b":status" && !name.eq_ignore_ascii_case(b"content-type") {
                h3_headers.push(h3::Header::new(name, value));
            }
        }

        // ボディがあるかどうかを判定
        let has_body = body.is_some() && body.is_some_and(|b| !b.is_empty());

        // ボディがない場合はヘッダーのみ送信してトレイラーへ
        if let Err(e) = h3_conn.send_response(&mut self.conn, stream_id, &h3_headers, false) {
            warn!("[HTTP/3] gRPC send_response error: {}", e);
            return Ok(());
        }

        // 2. ボディ送信 (ボディがある場合のみ)
        if has_body {
            if let Some(body_data) = body {
                if let Err(e) = h3_conn.send_body(&mut self.conn, stream_id, body_data, false) {
                    warn!("[HTTP/3] gRPC send_body error: {}", e);
                }
            }
        }

        // 3. トレイラー送信 (grpc-status, grpc-message)
        self.send_grpc_trailers_internal(stream_id, grpc_status, grpc_message)
    }

    /// gRPC トレイラーを送信（内部ヘルパー）
    ///
    /// grpc-status と grpc-message をトレイラーとして送信し、ストリームを終了。
    #[cfg(feature = "grpc")]
    fn send_grpc_trailers_internal(
        &mut self,
        stream_id: u64,
        grpc_status: u32,
        grpc_message: Option<&str>,
    ) -> io::Result<()> {
        use crate::grpc::status::{GrpcStatus, GrpcStatusCode};

        let h3_conn = match &mut self.h3_conn {
            Some(h3) => h3,
            None => return Ok(()),
        };

        let code = GrpcStatusCode::from_u8(grpc_status as u8).unwrap_or(GrpcStatusCode::Unknown);

        let status = if let Some(msg) = grpc_message {
            GrpcStatus::error(code, msg)
        } else {
            GrpcStatus::from_code(code)
        };
        let trailer_pairs = status.to_trailers();

        // quiche の H3 トレイラー送信
        let trailers: Vec<h3::Header> = trailer_pairs
            .iter()
            .map(|(n, v)| h3::Header::new(n.as_slice(), v.as_slice()))
            .collect();

        // send_response with fin=true acts as trailers in HTTP/3
        // However, quiche doesn't have a direct send_trailers API
        // We need to use send_body with fin=true after all data
        // For now, use send_response as trailers-only frame
        if let Err(e) = h3_conn.send_response(&mut self.conn, stream_id, &trailers, true) {
            // Note: This might not work for all cases, but quiche's h3 API is limited
            debug!("[HTTP/3] gRPC trailers send attempt: {:?}", e);
        }

        Ok(())
    }

    /// プロキシ処理（HTTP/1.1またはHTTP/2バックエンドへの変換）
    ///
    /// HTTP/3からのリクエストをバックエンドに転送します。
    /// バックエンドがHTTP/3に対応していない場合は、HTTP/2またはHTTP/1.1にフォールバックします。
    async fn handle_proxy(
        &mut self,
        stream_id: u64,
        upstream_group: &Arc<UpstreamGroup>,
        compression: &CompressionConfig,
        client_encoding: AcceptedEncoding,
        method: &[u8],
        req_path: &[u8],
        prefix: &[u8],
        headers: &[h3::Header],
        request_body: &[u8],
    ) -> io::Result<(u16, usize)> {
        // サーバー選択
        let server = match upstream_group.select(&self.client_ip) {
            Some(s) => s,
            None => {
                self.send_error_response(stream_id, 502, b"Bad Gateway")?;
                return Ok((502, 11));
            }
        };

        server.acquire();
        let target = &server.target;

        // リクエストパス構築
        let path_str = std::str::from_utf8(req_path).unwrap_or("/");
        let sub_path = if prefix.is_empty() {
            path_str.to_string()
        } else {
            let prefix_str = std::str::from_utf8(prefix).unwrap_or("");
            if let Some(remaining) = path_str.strip_prefix(prefix_str) {
                let base = target.path_prefix.trim_end_matches('/');

                if remaining.is_empty() {
                    if base.is_empty() {
                        "/".to_string()
                    } else {
                        format!("{}/", base)
                    }
                } else if remaining.starts_with('/') {
                    if base.is_empty() {
                        remaining.to_string()
                    } else {
                        format!("{}{}", base, remaining)
                    }
                } else {
                    if base.is_empty() {
                        format!("/{}", remaining)
                    } else {
                        format!("{}/{}", base, remaining)
                    }
                }
            } else {
                path_str.to_string()
            }
        };

        let final_path = if sub_path.is_empty() { "/" } else { &sub_path };

        // HTTP/1.1 リクエスト構築
        let mut request = Vec::with_capacity(1024 + request_body.len());
        request.extend_from_slice(method);
        request.extend_from_slice(b" ");
        request.extend_from_slice(final_path.as_bytes());
        request.extend_from_slice(b" HTTP/1.1\r\nHost: ");
        request.extend_from_slice(target.host.as_bytes());

        if !target.is_default_port() {
            request.extend_from_slice(b":");
            let mut port_buf = itoa::Buffer::new();
            request.extend_from_slice(port_buf.format(target.port).as_bytes());
        }
        request.extend_from_slice(b"\r\n");

        // ヘッダー追加（疑似ヘッダー以外）
        for header in headers {
            if header.name().starts_with(b":") {
                continue;
            }
            if header.name().eq_ignore_ascii_case(b"connection")
                || header.name().eq_ignore_ascii_case(b"keep-alive")
                || header.name().eq_ignore_ascii_case(b"transfer-encoding")
            {
                continue;
            }
            request.extend_from_slice(header.name());
            request.extend_from_slice(b": ");
            request.extend_from_slice(header.value());
            request.extend_from_slice(b"\r\n");
        }

        // Content-Length 追加
        if !request_body.is_empty() {
            request.extend_from_slice(b"Content-Length: ");
            let mut len_buf = itoa::Buffer::new();
            request.extend_from_slice(len_buf.format(request_body.len()).as_bytes());
            request.extend_from_slice(b"\r\n");
        }

        request.extend_from_slice(b"Connection: close\r\n\r\n");
        request.extend_from_slice(request_body);

        // 非同期プロキシ処理（monoio TcpStream使用）
        // io_uringベースの非同期I/Oでバックエンド通信を行う
        let timeout_secs = 30;
        let tls_insecure = upstream_group.tls_insecure();
        let proxy_result =
            proxy_to_backend_async_with_tls(target, request, timeout_secs, tls_insecure).await;

        server.release();

        match proxy_result {
            Ok(backend_result) => {
                // バックエンド結果からHTTP/3レスポンスを構築
                let status_code = backend_result.status_code;

                // 圧縮判定
                let mut content_type: Option<&[u8]> = None;
                let mut existing_encoding: Option<&[u8]> = None;
                for (name, value) in &backend_result.headers {
                    if name.eq_ignore_ascii_case(b"content-type") {
                        content_type = Some(value.as_slice());
                    } else if name.eq_ignore_ascii_case(b"content-encoding") {
                        existing_encoding = Some(value.as_slice());
                    }
                }

                let should_compress = compression.should_compress(
                    client_encoding,
                    content_type,
                    Some(backend_result.body.len()),
                    existing_encoding,
                );

                // レスポンスヘッダーを構築（ホップバイホップヘッダーをスキップ）
                let mut resp_headers: Vec<(&[u8], &[u8])> = Vec::new();
                for (name, value) in &backend_result.headers {
                    if name.eq_ignore_ascii_case(b"connection")
                        || name.eq_ignore_ascii_case(b"transfer-encoding")
                        || name.eq_ignore_ascii_case(b"keep-alive")
                    {
                        continue;
                    }
                    // 圧縮時は Content-Length と Content-Encoding をスキップ
                    if should_compress.is_some()
                        && (name.eq_ignore_ascii_case(b"content-length")
                            || name.eq_ignore_ascii_case(b"content-encoding"))
                    {
                        continue;
                    }
                    resp_headers.push((name.as_slice(), value.as_slice()));
                }

                // 圧縮処理
                let response_body = if let Some(enc) = should_compress {
                    compress_body_h3(&backend_result.body, enc, compression)
                } else {
                    backend_result.body.clone()
                };

                self.send_response(stream_id, status_code, &resp_headers, Some(&response_body))?;
                Ok((status_code, response_body.len()))
            }
            Err(e) => {
                warn!("[HTTP/3] Async backend proxy error: {}", e);
                self.send_error_response(stream_id, 502, b"Bad Gateway")?;
                Ok((502, 11))
            }
        }
    }

    /// ファイル配信
    async fn handle_sendfile(
        &mut self,
        stream_id: u64,
        base_path: &Path,
        is_dir: bool,
        index_file: Option<&str>,
        req_path: &[u8],
        prefix: &[u8],
        security: &SecurityConfig,
    ) -> io::Result<(u16, usize)> {
        let path_str = std::str::from_utf8(req_path).unwrap_or("/");
        let prefix_str = std::str::from_utf8(prefix).unwrap_or("");

        // プレフィックス除去後のサブパス
        let sub_path = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
            &path_str[prefix_str.len()..]
        } else {
            path_str
        };

        let clean_sub = sub_path.trim_start_matches('/');

        // パストラバーサル防止
        if clean_sub.contains("..") {
            self.send_error_response(stream_id, 403, b"Forbidden")?;
            return Ok((403, 9));
        }

        // ファイルパス構築
        let file_path = if is_dir {
            let mut p = base_path.to_path_buf();
            if clean_sub.is_empty() || clean_sub == "/" {
                p.push(index_file.unwrap_or("index.html"));
            } else {
                p.push(clean_sub);
                if p.is_dir() {
                    p.push(index_file.unwrap_or("index.html"));
                }
            }
            p
        } else {
            if !clean_sub.is_empty() {
                self.send_error_response(stream_id, 404, b"Not Found")?;
                return Ok((404, 9));
            }
            base_path.to_path_buf()
        };

        // ファイル読み込み（B-26: whole-file の同期 read はイベントループをブロックするため
        // offload（専用スレッドプール）へ退避する。HTTP/1.1 経路の proxy.rs と同方式）。
        let read_path = file_path.clone();
        // 理由付き allow: offload ワーカースレッド内で実行（イベントループ非ブロック）。
        #[allow(clippy::disallowed_methods)]
        let read_result = crate::runtime::offload::offload(move || std::fs::read(read_path)).await;
        let data = match read_result {
            Ok(d) => d,
            Err(_) => {
                self.send_error_response(stream_id, 404, b"Not Found")?;
                return Ok((404, 9));
            }
        };

        let mime_type = mime_guess::from_path(&file_path).first_or_octet_stream();
        let mime_str = mime_type.as_ref();

        let mut resp_headers: Vec<(&[u8], &[u8])> = vec![
            (b"content-type", mime_str.as_bytes()),
            (b"server", b"veil/http3"),
        ];

        // セキュリティヘッダー追加
        let security_headers: Vec<(Vec<u8>, Vec<u8>)> = security
            .add_response_headers
            .iter()
            .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
            .collect();

        for (k, v) in &security_headers {
            resp_headers.push((k.as_slice(), v.as_slice()));
        }

        self.send_response(stream_id, 200, &resp_headers, Some(&data))?;
        Ok((200, data.len()))
    }

    /// リダイレクト処理
    fn handle_redirect(
        &mut self,
        stream_id: u64,
        redirect_url: &str,
        status_code: u16,
        preserve_path: bool,
        req_path: &[u8],
        prefix: &[u8],
    ) -> io::Result<(u16, usize)> {
        let path_str = std::str::from_utf8(req_path).unwrap_or("/");
        let prefix_str = std::str::from_utf8(prefix).unwrap_or("");

        // パス部分（prefix除去後）
        let sub_path = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
            &path_str[prefix_str.len()..]
        } else {
            path_str
        };

        // 変数置換とパス追加
        let mut final_url = redirect_url
            .replace("$request_uri", path_str)
            .replace("$path", sub_path);

        if preserve_path && !sub_path.is_empty() {
            if final_url.ends_with('/') && sub_path.starts_with('/') {
                final_url.push_str(&sub_path[1..]);
            } else if !final_url.ends_with('/') && !sub_path.starts_with('/') {
                final_url.push('/');
                final_url.push_str(sub_path);
            } else {
                final_url.push_str(sub_path);
            }
        }

        self.send_response(
            stream_id,
            status_code,
            &[
                (b"location", final_url.as_bytes()),
                (b"server", b"veil/http3"),
            ],
            None,
        )?;

        Ok((status_code, 0))
    }

    /// 部分的なレスポンスをフラッシュ
    fn flush_partial_responses(&mut self) -> io::Result<()> {
        let h3_conn = match &mut self.h3_conn {
            Some(h3) => h3,
            None => return Ok(()),
        };

        let mut completed = Vec::new();
        for (&stream_id, (body, written)) in &mut self.partial_responses {
            if *written < body.len() {
                match h3_conn.send_body(&mut self.conn, stream_id, &body[*written..], true) {
                    Ok(sent) => {
                        *written += sent;
                        if *written >= body.len() {
                            completed.push(stream_id);
                        }
                    }
                    Err(h3::Error::Done) => {}
                    Err(e) => {
                        warn!("[HTTP/3] send_body error: {}", e);
                        completed.push(stream_id);
                    }
                }
            } else {
                completed.push(stream_id);
            }
        }
        for stream_id in completed {
            self.partial_responses.remove(&stream_id);
        }

        Ok(())
    }

    /// 書き込み可能なストリームを処理（quiche パターン）
    ///
    /// conn.writable() で書き込み可能になったストリームに対して、
    /// 保留中の部分レスポンスを再送します。
    fn handle_writable_streams(&mut self) -> io::Result<()> {
        let h3_conn = match &mut self.h3_conn {
            Some(h3) => h3,
            None => return Ok(()),
        };

        // 書き込み可能なストリームを収集
        let writable_streams: Vec<u64> = self.conn.writable().collect();

        for stream_id in writable_streams {
            // 部分レスポンスがあるかチェック
            if let Some((body, written)) = self.partial_responses.get_mut(&stream_id) {
                if *written < body.len() {
                    match h3_conn.send_body(&mut self.conn, stream_id, &body[*written..], true) {
                        Ok(sent) => {
                            debug!(
                                "[HTTP/3] Writable stream {}: sent {} more bytes ({}/{})",
                                stream_id,
                                sent,
                                *written + sent,
                                body.len()
                            );
                            *written += sent;
                        }
                        Err(h3::Error::Done) => {
                            // まだブロックされている
                            debug!("[HTTP/3] Stream {} still blocked", stream_id);
                        }
                        Err(e) => {
                            warn!(
                                "[HTTP/3] send_body error on writable stream {}: {}",
                                stream_id, e
                            );
                        }
                    }
                }
            }
        }

        // 完了したストリームを削除
        self.partial_responses
            .retain(|_, (body, written)| *written < body.len());

        Ok(())
    }
}

// ====================
// F-32: ストリーミング用フリー関数（リクエスト head 構築・パス計算・ストリーム駆動）
// ====================

/// プレフィックス除去 + `path_prefix` 連結でバックエンドへ送るパスを構築する。
/// `handle_proxy` と同一ロジック（挙動を一致させるため共有）。
fn compute_backend_path(target: &ProxyTarget, req_path: &[u8], prefix: &[u8]) -> String {
    let path_str = std::str::from_utf8(req_path).unwrap_or("/");
    let sub_path = if prefix.is_empty() {
        path_str.to_string()
    } else {
        let prefix_str = std::str::from_utf8(prefix).unwrap_or("");
        if let Some(remaining) = path_str.strip_prefix(prefix_str) {
            let base = target.path_prefix.trim_end_matches('/');
            if remaining.is_empty() {
                if base.is_empty() {
                    "/".to_string()
                } else {
                    format!("{}/", base)
                }
            } else if remaining.starts_with('/') {
                if base.is_empty() {
                    remaining.to_string()
                } else {
                    format!("{}{}", base, remaining)
                }
            } else if base.is_empty() {
                format!("/{}", remaining)
            } else {
                format!("{}/{}", base, remaining)
            }
        } else {
            path_str.to_string()
        }
    };
    if sub_path.is_empty() {
        "/".to_string()
    } else {
        sub_path
    }
}

/// HTTP/1.1 リクエスト head（リクエストライン + ヘッダ、**末尾の空行は含めない**）を構築する。
///
/// ボディフレーミング（`Transfer-Encoding: chunked` か無しか）と末尾の空行は、**実際に
/// ボディデータが来たか**をバックエンドタスクが判定してから付与する（HTTP/3 では HEADERS
/// 受信時点でボディ有無が確定しないため。例: h3 クライアントが HEADERS と fin を別送する GET は
/// `more_frames=true` でもボディなし）。`Connection: close` で 1 リクエスト 1 接続。
fn build_h1_request_head(
    target: &ProxyTarget,
    method: &[u8],
    final_path: &str,
    headers: &[h3::Header],
) -> Vec<u8> {
    let mut req = Vec::with_capacity(512);
    req.extend_from_slice(method);
    req.push(b' ');
    req.extend_from_slice(final_path.as_bytes());
    req.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    req.extend_from_slice(target.host.as_bytes());
    if !target.is_default_port() {
        req.push(b':');
        let mut port_buf = itoa::Buffer::new();
        req.extend_from_slice(port_buf.format(target.port).as_bytes());
    }
    req.extend_from_slice(b"\r\n");

    for header in headers {
        let name = header.name();
        // B-11: expect はプロキシが終端する（ボディを無条件転送するため、バックエンドに
        // 100 Continue 中間応答を出させない）。
        if name.starts_with(b":")
            || name.eq_ignore_ascii_case(b"connection")
            || name.eq_ignore_ascii_case(b"keep-alive")
            || name.eq_ignore_ascii_case(b"transfer-encoding")
            || name.eq_ignore_ascii_case(b"content-length")
            || name.eq_ignore_ascii_case(b"expect")
        {
            continue;
        }
        req.extend_from_slice(name);
        req.extend_from_slice(b": ");
        req.extend_from_slice(header.value());
        req.extend_from_slice(b"\r\n");
    }

    // ボディフレーミングと末尾空行はタスク側で付与する。
    req.extend_from_slice(b"Connection: close\r\n");
    req
}

/// 1 ストリームの駆動結果。`true` を返したら呼び出し側が `proxy_streams` から除去する。
fn drive_proxy_stream(
    h3: &mut h3::Connection,
    conn: &mut quiche::Connection,
    stream_id: u64,
    ps: &mut ProxyStream,
) -> bool {
    drive_request_pump(h3, conn, stream_id, ps);
    drive_response_flush(h3, conn, stream_id, ps);
    // 完了条件: レスポンス fin 送出済み かつ リクエスト側クローズ済み。
    ps.resp_fin_sent && ps.req_tx.is_none() && ps.req_pending.is_empty()
}

/// リクエストボディ pump: `recv_body` → req チャネル（フロー制御 + バックプレッシャ）。
fn drive_request_pump(
    h3: &mut h3::Connection,
    conn: &mut quiche::Connection,
    stream_id: u64,
    ps: &mut ProxyStream,
) {
    use crate::http3_stream::TrySendError;
    let tx = match &ps.req_tx {
        Some(t) => t,
        None => return,
    };

    // ボディ上限超過済みなら何もしない（応答 flush 側で 413 + リセット）。
    if ps.req_too_large {
        return;
    }

    // 1. 未投入ボディ（初回バッチ/溢れ分）を先に流す（ゼロコピー: clone せず move）。
    while let Some(front) = ps.req_pending.pop_front() {
        match tx.try_send(front) {
            Ok(()) => {}
            Err(TrySendError::Full(item)) => {
                ps.req_pending.push_front(item); // バックプレッシャ: recv_body も止める。
                return;
            }
            Err(TrySendError::Closed(_)) => {
                ps.req_pending.clear();
                ps.req_tx = None;
                return;
            }
        }
    }

    // 2. quiche から recv_body してチャネルへ（容量がある間だけ = バックプレッシャ）。
    if ps.req_readable {
        loop {
            if tx.is_full() {
                return; // データを quiche に残す → フロー制御でクライアント送信が止まる。
            }
            let mut buf = BytesMut::with_capacity(REQ_RECV_CHUNK);
            let spare = buf.spare_capacity_mut();
            // SAFETY: recv_body は read バイトのみ初期化。advance_mut で len に反映。
            let spare_u8 = unsafe {
                std::slice::from_raw_parts_mut(spare.as_mut_ptr() as *mut u8, spare.len())
            };
            match h3.recv_body(conn, stream_id, spare_u8) {
                Ok(n) if n > 0 => {
                    unsafe { buf.advance_mut(n) };
                    ps.req_bytes_total += n as u64;
                    // ボディ上限チェック（0 = 無制限）。
                    if ps.max_request_body > 0 && ps.req_bytes_total > ps.max_request_body {
                        ps.req_too_large = true;
                        ps.req_tx = None; // バックエンドタスクを中断。
                        ps.req_pending.clear();
                        // クライアントの送信を止める。
                        let _ = conn.stream_shutdown(stream_id, quiche::Shutdown::Read, 0);
                        return;
                    }
                    match tx.try_send(buf.freeze()) {
                        Ok(()) => continue,
                        Err(TrySendError::Full(b)) => {
                            ps.req_pending.push_back(b);
                            return;
                        }
                        Err(TrySendError::Closed(_)) => {
                            ps.req_tx = None;
                            return;
                        }
                    }
                }
                Ok(_) | Err(h3::Error::Done) => {
                    ps.req_readable = false;
                    break;
                }
                Err(e) => {
                    debug!("[HTTP/3] recv_body (stream) error: {}", e);
                    ps.req_readable = false;
                    break;
                }
            }
        }
    }

    // B-12: fin を含む最終データを本 pump の `recv_body`（`h3.poll()` の外）で消費した場合、
    // h3 の `Finished` イベントは内部キューに積まれるが、`poll` はパケット受信時にしか
    // 呼ばれないため、クライアントが送信を終えると新規パケットが来ず永久に取り出されない
    // （EOF 未伝播 → バックエンドタスクが待機 → レスポンス無し → QUIC アイドルタイムアウト）。
    // トランスポート層の `stream_finished`（fin 受信済みかつ全データ消費済み）を直接確認して
    // EOF を伝播する。
    if !ps.req_eof_seen && conn.stream_finished(stream_id) {
        ps.req_eof_seen = true;
    }

    // 3. クライアント END_STREAM 受信かつ全消化なら送信端を閉じて EOF 伝播。
    if ps.req_eof_seen && ps.req_pending.is_empty() && !ps.req_readable {
        ps.req_tx = None;
    }
}

/// レスポンス flush: resp チャネル → `send_response`/`send_body`（フロー制御 + 部分送信保持）。
fn drive_response_flush(
    h3: &mut h3::Connection,
    conn: &mut quiche::Connection,
    stream_id: u64,
    ps: &mut ProxyStream,
) {
    use crate::http3_stream::{RespMsg, TryRecv};

    if ps.resp_fin_sent {
        return;
    }

    // ボディ上限超過 → 413 を返して終了（応答未開始時のみ）。
    if ps.req_too_large && !ps.resp_started {
        send_simple_h3_error(h3, conn, stream_id, 413);
        ps.resp_fin_sent = true;
        return;
    }

    // 0. 保留中の fin を再送。
    if ps.need_fin {
        if try_send_h3_fin(h3, conn, stream_id) {
            ps.resp_fin_sent = true;
            ps.need_fin = false;
        }
        return;
    }

    // 1. StreamBlocked で保留した head を再送。
    if let Some((status, headers)) = ps.head_pending.take() {
        match send_h3_head(h3, conn, stream_id, status, &headers) {
            HeadSend::Sent => ps.resp_started = true,
            HeadSend::Blocked => {
                ps.head_pending = Some((status, headers));
                return;
            }
            HeadSend::Error => {
                ps.resp_fin_sent = true;
                return;
            }
        }
    }

    // 2. 部分送信のボディ断片を flush。
    if let Some((buf, off)) = ps.body_pending.take() {
        match send_h3_body(h3, conn, stream_id, &buf, off) {
            BodySend::Done => {}
            BodySend::Partial(new_off) => {
                ps.body_pending = Some((buf, new_off));
                return;
            }
            BodySend::Blocked => {
                ps.body_pending = Some((buf, off));
                return;
            }
            BodySend::Error => {
                ps.resp_fin_sent = true;
                return;
            }
        }
    }

    // 3. チャネルを排出して送出。
    loop {
        if ps.head_pending.is_some() || ps.body_pending.is_some() {
            return;
        }
        match ps.resp_rx.try_recv() {
            TryRecv::Item(RespMsg::Head { status, headers }) => {
                match send_h3_head(h3, conn, stream_id, status, &headers) {
                    HeadSend::Sent => ps.resp_started = true,
                    HeadSend::Blocked => {
                        ps.head_pending = Some((status, headers));
                        return;
                    }
                    HeadSend::Error => {
                        ps.resp_fin_sent = true;
                        return;
                    }
                }
            }
            TryRecv::Item(RespMsg::Body(b)) => match send_h3_body(h3, conn, stream_id, &b, 0) {
                BodySend::Done => {}
                BodySend::Partial(off) => {
                    ps.body_pending = Some((b, off));
                    return;
                }
                BodySend::Blocked => {
                    ps.body_pending = Some((b, 0));
                    return;
                }
                BodySend::Error => {
                    ps.resp_fin_sent = true;
                    return;
                }
            },
            TryRecv::Item(RespMsg::Error { status }) => {
                if !ps.resp_started {
                    send_simple_h3_error(h3, conn, stream_id, status);
                } else {
                    // 応答途中のエラー: ストリームをリセット。
                    let _ = conn.stream_shutdown(stream_id, quiche::Shutdown::Write, 0x10c);
                }
                ps.resp_fin_sent = true;
                return;
            }
            TryRecv::Closed => {
                // バックエンド完了 → fin 送出。
                if ps.resp_started {
                    if try_send_h3_fin(h3, conn, stream_id) {
                        ps.resp_fin_sent = true;
                    } else {
                        ps.need_fin = true;
                    }
                } else {
                    // head を一度も生成できなかった → 502。
                    send_simple_h3_error(h3, conn, stream_id, 502);
                    ps.resp_fin_sent = true;
                }
                return;
            }
            TryRecv::Empty => return,
        }
    }
}

/// head 送出の結果。
enum HeadSend {
    Sent,
    Blocked,
    Error,
}

/// body 送出の結果。
enum BodySend {
    Done,
    Partial(usize),
    Blocked,
    Error,
}

/// レスポンス head（`:status` + ヘッダ）を `send_response(fin=false)` で送る。
fn send_h3_head(
    h3: &mut h3::Connection,
    conn: &mut quiche::Connection,
    stream_id: u64,
    status: u16,
    headers: &[(Bytes, Bytes)],
) -> HeadSend {
    let mut status_buf = itoa::Buffer::new();
    let status_str = status_buf.format(status);
    let mut h3_headers: Vec<h3::Header> = Vec::with_capacity(headers.len() + 2);
    h3_headers.push(h3::Header::new(b":status", status_str.as_bytes()));
    h3_headers.push(h3::Header::new(b"server", b"veil/http3"));
    for (name, value) in headers {
        if name.eq_ignore_ascii_case(b":status") || name.eq_ignore_ascii_case(b"server") {
            continue;
        }
        h3_headers.push(h3::Header::new(name, value));
    }
    match h3.send_response(conn, stream_id, &h3_headers, false) {
        Ok(()) => HeadSend::Sent,
        Err(h3::Error::StreamBlocked) => HeadSend::Blocked,
        Err(e) => {
            warn!("[HTTP/3] streaming send_response error: {}", e);
            HeadSend::Error
        }
    }
}

/// ボディ断片を `send_body(fin=false)` で送る（`off` から）。
fn send_h3_body(
    h3: &mut h3::Connection,
    conn: &mut quiche::Connection,
    stream_id: u64,
    buf: &[u8],
    off: usize,
) -> BodySend {
    if off >= buf.len() {
        return BodySend::Done;
    }
    match h3.send_body(conn, stream_id, &buf[off..], false) {
        Ok(n) => {
            let new_off = off + n;
            if new_off >= buf.len() {
                BodySend::Done
            } else {
                BodySend::Partial(new_off)
            }
        }
        Err(h3::Error::Done) => BodySend::Blocked, // 送信バッファ/フロー制御で送れない。
        Err(e) => {
            warn!("[HTTP/3] streaming send_body error: {}", e);
            BodySend::Error
        }
    }
}

/// 空ボディ + fin を送る。`true` で fin 送出完了、`false` でブロック（再試行）。
fn try_send_h3_fin(h3: &mut h3::Connection, conn: &mut quiche::Connection, stream_id: u64) -> bool {
    match h3.send_body(conn, stream_id, b"", true) {
        Ok(_) => true,
        Err(h3::Error::Done) => false,
        Err(e) => {
            debug!("[HTTP/3] streaming fin send error: {}", e);
            true // これ以上どうにもならないため完了扱い。
        }
    }
}

/// 簡易エラーレスポンス（head + 小ボディ + fin）を送る。
fn send_simple_h3_error(
    h3: &mut h3::Connection,
    conn: &mut quiche::Connection,
    stream_id: u64,
    status: u16,
) {
    let mut status_buf = itoa::Buffer::new();
    let status_str = status_buf.format(status);
    let body: &[u8] = match status {
        413 => b"Payload Too Large",
        502 => b"Bad Gateway",
        504 => b"Gateway Timeout",
        _ => b"Error",
    };
    let mut len_buf = itoa::Buffer::new();
    let h3_headers = [
        h3::Header::new(b":status", status_str.as_bytes()),
        h3::Header::new(b"server", b"veil/http3"),
        h3::Header::new(b"content-type", b"text/plain"),
        h3::Header::new(b"content-length", len_buf.format(body.len()).as_bytes()),
    ];
    match h3.send_response(conn, stream_id, &h3_headers, false) {
        Ok(()) => {
            let _ = h3.send_body(conn, stream_id, body, true);
        }
        Err(e) => debug!("[HTTP/3] streaming error response send failed: {}", e),
    }
}

// ====================
// 非同期バックエンドプロキシ（monoio TcpStream 使用）
// ====================

/// バックエンドプロキシ結果
pub struct BackendProxyResult {
    /// HTTPステータスコード
    pub status_code: u16,
    /// レスポンスボディ
    pub body: Vec<u8>,
    /// レスポンスヘッダー（(name, value) のペア）
    pub headers: Vec<(Vec<u8>, Vec<u8>)>,
}

pub(crate) async fn proxy_to_backend_async_with_tls(
    target: &ProxyTarget,
    request: Vec<u8>,
    timeout_secs: u64,
    tls_insecure: bool,
) -> io::Result<BackendProxyResult> {
    use crate::runtime::tcp::TcpStream;
    use std::os::unix::io::AsRawFd;

    let addr = format!("{}:{}", target.host, target.port);
    debug!("[HTTP/3] Async connecting to backend {}", addr);

    // 非同期TCP接続（タイムアウト付き）
    let connect_future = TcpStream::connect_str(&addr);
    let backend = match crate::runtime::time::timeout(
        Duration::from_secs(timeout_secs),
        connect_future,
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            warn!("[HTTP/3] Async backend connect error: {}", e);
            return Err(e);
        }
        Err(_) => {
            warn!("[HTTP/3] Async backend connect timeout");
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Backend connect timeout",
            ));
        }
    };

    debug!("[HTTP/3] Async connected to backend {}", addr);
    let _ = backend.set_nodelay(true);

    // TLSバックエンドの場合
    if target.use_tls {
        return proxy_to_tls_backend_async(target, request, backend, timeout_secs, tls_insecure)
            .await;
    }

    let fd = backend.as_raw_fd();

    // リクエスト送信（非同期）
    let mut written = 0;
    while written < request.len() {
        match write_nonblocking(fd, &request[written..]) {
            Ok(n) if n > 0 => written += n,
            Ok(_) => {
                backend.writable().await?;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                backend.writable().await?;
            }
            Err(e) => return Err(e),
        }
    }

    debug!("[HTTP/3] Async request sent: {} bytes", written);

    // レスポンス受信（非同期）
    let mut response = Vec::with_capacity(16384);
    let mut buf = vec![0u8; 8192];
    let read_timeout = Duration::from_secs(timeout_secs);
    let start_time = std::time::Instant::now();

    loop {
        if start_time.elapsed() > read_timeout {
            break;
        }

        match read_nonblocking(fd, &mut buf) {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                let remaining = read_timeout.saturating_sub(start_time.elapsed());
                if remaining.is_zero() {
                    break;
                }
                match crate::runtime::time::timeout(remaining, backend.readable()).await {
                    Ok(Ok(())) => continue,
                    Ok(Err(e)) if response.is_empty() => return Err(e),
                    _ => break,
                }
            }
            Err(e) if response.is_empty() => return Err(e),
            Err(_) => break,
        }
    }

    debug!("[HTTP/3] Async response received: {} bytes", response.len());
    parse_http_response(&response)
}

/// TLSバックエンドへの非同期プロキシ処理（kTLS版）
/// kTLS/rustlsフォールバック問題を回避するため spawn_blocking で std TLS 接続を使用
#[cfg(feature = "ktls")]
// 理由付き allow: 同期 connect/TLS は std::thread::spawn した専用スレッド内で実行し、結果を mpsc + ポーリングで受け取る（イベントループ非ブロック）。
#[allow(clippy::disallowed_methods)]
async fn proxy_to_tls_backend_async(
    target: &ProxyTarget,
    request: Vec<u8>,
    tcp_stream: crate::runtime::tcp::TcpStream,
    timeout_secs: u64,
    tls_insecure: bool,
) -> io::Result<BackendProxyResult> {
    // monoio TcpStream は不要（別スレッドで std::net::TcpStream を使うため）
    drop(tcp_stream);

    let skip_verify = tls_insecure;
    let addr = format!("{}:{}", target.host, target.port);
    let sni_name = target
        .sni_name
        .as_deref()
        .unwrap_or(&target.host)
        .to_string();

    use rustls::ClientConfig;
    use std::sync::Arc;

    let config: Arc<ClientConfig> = if skip_verify {
        #[derive(Debug)]
        struct NoVerify;
        impl rustls::client::danger::ServerCertVerifier for NoVerify {
            fn verify_server_cert(
                &self,
                _: &rustls::pki_types::CertificateDer<'_>,
                _: &[rustls::pki_types::CertificateDer<'_>],
                _: &rustls::pki_types::ServerName<'_>,
                _: &[u8],
                _: rustls::pki_types::UnixTime,
            ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            }
            fn verify_tls12_signature(
                &self,
                _: &[u8],
                _: &rustls::pki_types::CertificateDer<'_>,
                _: &rustls::DigitallySignedStruct,
            ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
            {
                Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
            }
            fn verify_tls13_signature(
                &self,
                _: &[u8],
                _: &rustls::pki_types::CertificateDer<'_>,
                _: &rustls::DigitallySignedStruct,
            ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
            {
                Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
            }
            fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
                rustls::crypto::aws_lc_rs::default_provider()
                    .signature_verification_algorithms
                    .supported_schemes()
                    .to_vec()
            }
        }
        Arc::new(
            ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerify))
                .with_no_client_auth(),
        )
    } else {
        let root_store = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        Arc::new(
            ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth(),
        )
    };

    // 別スレッドでブロッキング TLS 通信を実行し、mpsc channel 経由で結果を受け取る
    let (tx, rx) = std::sync::mpsc::sync_channel::<io::Result<BackendProxyResult>>(1);
    std::thread::spawn(move || {
        use std::io::Write;
        let result = (|| -> io::Result<BackendProxyResult> {
            let timeout = Duration::from_secs(timeout_secs);
            let mut std_stream = std::net::TcpStream::connect(&addr as &str).map_err(|e| {
                warn!("[HTTP/3] std backend connect error: {}", e);
                e
            })?;
            std_stream.set_read_timeout(Some(timeout))?;
            std_stream.set_write_timeout(Some(timeout))?;
            let server_name = rustls::pki_types::ServerName::try_from(sni_name)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
            let mut conn = rustls::ClientConnection::new(config, server_name)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            let mut tls = rustls::Stream::new(&mut conn, &mut std_stream);
            tls.write_all(&request)?;
            let mut response = Vec::with_capacity(16384);
            let mut buf = [0u8; 8192];
            // UnexpectedEof は TLS close_notify なしの正常な接続終了（HTTP/1.1 バックエンドで一般的）
            loop {
                match std::io::Read::read(&mut tls, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => response.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                    Err(e) => return Err(e),
                }
            }
            parse_http_response(&response)
        })();
        let _ = tx.send(result);
    });

    // try_recv でポーリング（バックエンドが同一ホスト上のため数 ms で完了）
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        match rx.try_recv() {
            Ok(result) => return result,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                return Err(io::Error::other("backend thread died"));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                if std::time::Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "backend TLS timeout",
                    ));
                }
                crate::runtime::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }
}

/// TLSバックエンドへの非同期プロキシ処理（non-kTLS）
/// 別スレッドでブロッキング TLS 通信を行う
#[cfg(not(feature = "ktls"))]
async fn proxy_to_tls_backend_async(
    target: &ProxyTarget,
    request: Vec<u8>,
    tcp_stream: crate::runtime::tcp::TcpStream,
    timeout_secs: u64,
    tls_insecure: bool,
) -> io::Result<BackendProxyResult> {
    use rustls::ClientConfig;
    use std::sync::Arc;

    // monoio TcpStream は不要（別スレッドで std::net::TcpStream を使うため）
    drop(tcp_stream);

    let skip_verify = tls_insecure;
    let addr = format!("{}:{}", target.host, target.port);
    let sni_name = target
        .sni_name
        .as_deref()
        .unwrap_or(&target.host)
        .to_string();

    let config: Arc<ClientConfig> = if skip_verify {
        #[derive(Debug)]
        struct NoVerify;
        impl rustls::client::danger::ServerCertVerifier for NoVerify {
            fn verify_server_cert(
                &self,
                _: &rustls::pki_types::CertificateDer<'_>,
                _: &[rustls::pki_types::CertificateDer<'_>],
                _: &rustls::pki_types::ServerName<'_>,
                _: &[u8],
                _: rustls::pki_types::UnixTime,
            ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            }
            fn verify_tls12_signature(
                &self,
                _: &[u8],
                _: &rustls::pki_types::CertificateDer<'_>,
                _: &rustls::DigitallySignedStruct,
            ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
            {
                Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
            }
            fn verify_tls13_signature(
                &self,
                _: &[u8],
                _: &rustls::pki_types::CertificateDer<'_>,
                _: &rustls::DigitallySignedStruct,
            ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
            {
                Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
            }
            fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
                rustls::crypto::aws_lc_rs::default_provider()
                    .signature_verification_algorithms
                    .supported_schemes()
                    .to_vec()
            }
        }
        Arc::new(
            ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerify))
                .with_no_client_auth(),
        )
    } else {
        let root_store = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        Arc::new(
            ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth(),
        )
    };

    // 別スレッドでブロッキング TLS 通信を実行し、mpsc channel 経由で結果を受け取る
    let (tx, rx) = std::sync::mpsc::sync_channel::<io::Result<BackendProxyResult>>(1);
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        let result = (|| -> io::Result<BackendProxyResult> {
            let timeout = Duration::from_secs(timeout_secs);
            let mut std_stream = std::net::TcpStream::connect(&addr as &str).map_err(|e| {
                warn!("[HTTP/3] std backend connect error: {}", e);
                e
            })?;
            std_stream.set_read_timeout(Some(timeout))?;
            std_stream.set_write_timeout(Some(timeout))?;
            let server_name = rustls::pki_types::ServerName::try_from(sni_name)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
            let mut conn = rustls::ClientConnection::new(config, server_name)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            let mut tls = rustls::Stream::new(&mut conn, &mut std_stream);
            tls.write_all(&request)?;
            let mut response = Vec::with_capacity(16384);
            tls.read_to_end(&mut response)?;
            parse_http_response(&response)
        })();
        let _ = tx.send(result);
    });

    // try_recv でポーリング（バックエンドが同一ホスト上のため数 ms で完了）
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        match rx.try_recv() {
            Ok(result) => return result,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                return Err(io::Error::new(io::ErrorKind::Other, "backend thread died"));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                if std::time::Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "backend TLS timeout",
                    ));
                }
                crate::runtime::time::sleep(Duration::from_millis(5)).await;
            }
        }
    }
}

#[inline]
fn read_nonblocking(fd: i32, buf: &mut [u8]) -> io::Result<usize> {
    let result = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

#[inline]
fn write_nonblocking(fd: i32, buf: &[u8]) -> io::Result<usize> {
    let result = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

fn parse_http_response(response: &[u8]) -> io::Result<BackendProxyResult> {
    let header_end = find_header_end(response)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Invalid HTTP response"))?;

    let header_bytes = &response[..header_end];
    let body = response[header_end + 4..].to_vec();
    let status_code = parse_status_code(header_bytes).unwrap_or(502);

    let mut headers = Vec::new();
    if let Some(first_crlf) = memchr::memchr(b'\n', header_bytes) {
        for line in header_bytes[first_crlf + 1..].split(|&b| b == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.is_empty() {
                continue;
            }
            if let Some(colon_pos) = memchr::memchr(b':', line) {
                let name = &line[..colon_pos];
                let value = line[colon_pos + 1..]
                    .strip_prefix(b" ")
                    .unwrap_or(&line[colon_pos + 1..]);
                if !name.eq_ignore_ascii_case(b"connection")
                    && !name.eq_ignore_ascii_case(b"transfer-encoding")
                    && !name.eq_ignore_ascii_case(b"keep-alive")
                {
                    headers.push((name.to_vec(), value.to_vec()));
                }
            }
        }
    }

    Ok(BackendProxyResult {
        status_code,
        body,
        headers,
    })
}

/// コネクション管理（Rc<RefCell> で共有）
type ConnectionMap = Rc<RefCell<HashMap<ConnectionId<'static>, Http3Handler>>>;

/// HTTP/3 サーバーを起動（monoio ランタイム上で実行）
///
/// この関数は monoio のスレッド内から呼び出す必要があります。
/// HTTP/1.1と同等のルーティング・セキュリティ・プロキシ機能をサポートします。
///
/// ## セキュリティ
/// 証明書データ（cert_pem, key_pem）は quiche へのロード完了後、
/// セキュアにゼロ化してからメモリから解放されます。
// clippy::await_holding_refcell_ref 許容理由: `connections`（Rc<RefCell<HashMap>>）を
// 借用するのは本 H3 メインループタスクのみ。バックエンドタスクは Rc チャネル + Notify
// 経由で通信し RefCell に触れない（F-32 のアクターモデル）ため、await 中に他タスクが
// 再入借用して panic する経路は存在しない（B-16 とは異なり単一借用者）。
#[allow(clippy::await_holding_refcell_ref)]
pub async fn run_http3_server_async(
    bind_addr: SocketAddr,
    mut config: Http3ServerConfig,
) -> io::Result<()> {
    // QUIC 設定を作成
    let mut quic_config = Config::new(quiche::PROTOCOL_VERSION)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;

    // TLS 証明書を設定
    // memfd アプローチ: 事前読み込み済みの PEM バイト列を memfd に書き込み、
    // /proc/self/fd/<fd> パス経由で quiche に渡す
    // これにより Landlock でファイルシステムアクセスを制限しながら HTTP/3 を使用可能
    //
    // セキュリティ: quiche が証明書を読み込んだ後:
    // 1. memfd を即座にドロップ（カーネルがメモリ解放）
    // 2. config 内の Vec<u8> をセキュアにゼロ化してからドロップ
    if let (Some(mut cert_pem), Some(mut key_pem)) = (config.cert_pem.take(), config.key_pem.take())
    {
        // memfd 経由でロード（Landlock 対応）
        info!("[HTTP/3] Loading certificates via memfd (Landlock compatible)");

        // 証明書を memfd に書き込み
        let (cert_memfd, cert_path) = create_memfd_for_pem("tls_cert", &cert_pem)
            .map_err(|e| io::Error::other(format!("Failed to create memfd for cert: {}", e)))?;

        // quiche が証明書を読み込む
        quic_config
            .load_cert_chain_from_pem_file(&cert_path)
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("cert load error (memfd): {}", e),
                )
            })?;

        // 証明書 memfd を即座にドロップ（fd を閉じてカーネルにメモリ解放を依頼）
        drop(cert_memfd);

        // 証明書データをセキュアにゼロ化
        secure_zero(&mut cert_pem);
        drop(cert_pem);
        debug!("[HTTP/3] Certificate data securely zeroed and released");

        // 秘密鍵を memfd に書き込み
        let (key_memfd, key_path) = create_memfd_for_pem("tls_key", &key_pem)
            .map_err(|e| io::Error::other(format!("Failed to create memfd for key: {}", e)))?;

        // quiche が秘密鍵を読み込む
        quic_config
            .load_priv_key_from_pem_file(&key_path)
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("key load error (memfd): {}", e),
                )
            })?;

        // 秘密鍵 memfd を即座にドロップ
        drop(key_memfd);

        // 秘密鍵データをセキュアにゼロ化
        secure_zero(&mut key_pem);
        drop(key_pem);
        debug!("[HTTP/3] Private key data securely zeroed and released");

        info!("[HTTP/3] Certificates loaded, memfd closed, sensitive data zeroed");
    } else {
        // ファイルパスから直接ロード（後方互換性）
        info!("[HTTP/3] Loading certificates from file path (legacy mode)");
        warn!("[HTTP/3] Note: When using Landlock, add cert/key paths to landlock_read_paths");

        quic_config
            .load_cert_chain_from_pem_file(&config.cert_path)
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("cert load error: {}", e),
                )
            })?;

        quic_config
            .load_priv_key_from_pem_file(&config.key_path)
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("key load error: {}", e),
                )
            })?;
    }

    // QUIC パラメータを設定
    quic_config.set_max_idle_timeout(config.max_idle_timeout);
    quic_config.set_max_recv_udp_payload_size(config.max_udp_payload_size as usize);
    quic_config.set_max_send_udp_payload_size(config.max_udp_payload_size as usize);
    quic_config.set_initial_max_data(config.initial_max_data);
    quic_config.set_initial_max_stream_data_bidi_local(config.initial_max_stream_data_bidi_local);
    quic_config.set_initial_max_stream_data_bidi_remote(config.initial_max_stream_data_bidi_remote);
    quic_config.set_initial_max_stream_data_uni(config.initial_max_stream_data_uni);
    quic_config.set_initial_max_streams_bidi(config.initial_max_streams_bidi);
    quic_config.set_initial_max_streams_uni(config.initial_max_streams_uni);
    quic_config.set_disable_active_migration(true);
    quic_config.enable_early_data();

    // HTTP/3 用の ALPN を設定
    quic_config
        .set_application_protos(h3::APPLICATION_PROTOCOL)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;

    // 設定を Rc で共有（quiche::Config は Clone できないため）
    let quic_config = Rc::new(RefCell::new(quic_config));

    // UDP ソケットを作成（monoio io_uring ベース）
    // SO_REUSEPORT を設定して複数ワーカーで並列処理を可能に
    // GSO/GRO は config.gso_gro_enabled に基づいて設定
    let socket = QuicUdpSocket::bind_reuseport_with_gso(bind_addr, config.gso_gro_enabled)?;
    info!(
        "[HTTP/3] GSO enabled: {}, GRO enabled: {} (config gso_gro_enabled: {})",
        socket.gso_enabled(),
        socket.gro_enabled(),
        config.gso_gro_enabled
    );
    let socket = Rc::new(socket);
    let local_addr = bind_addr;

    info!(
        "[HTTP/3] Server listening on {} (QUIC/UDP, monoio io_uring)",
        bind_addr
    );

    // コネクション管理
    let connections: ConnectionMap = Rc::new(RefCell::new(HashMap::new()));

    // F-32: バックエンドタスク → メインループの起床通知（全ハンドラ/タスクで共有）。
    let notify = crate::http3_stream::H3Notify::new();
    // F-46: バックエンドタスクの型付きプール（本ワーカースレッドの全接続で共有）。
    let backend_spawner = crate::http3_stream::backend_task_spawner();

    // 乱数生成器
    let rng = SystemRandom::new();

    // ルーティング設定を CURRENT_CONFIG から取得（ホットリロード対応）

    // F-33: 受信バッファを loop 外で一度だけ確保し再利用する。
    // 旧実装はデータグラム毎に 64KB の Vec を確保（さらに 2 回の to_vec コピー）していたが、
    // GRO 受信（recv_gro_async）+ スライス直渡しでヒープ確保とコピーを完全に排除する。
    // 64KB は単一 recvmsg の最大値。GRO 集約時はこのバッファに複数データグラムが詰めて返る。
    let mut recv_buf = vec![0u8; 65536];

    // メインループ: パケット受信とディスパッチ
    loop {
        // シャットダウンチェック
        if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
            info!("[HTTP/3] Initiating graceful shutdown...");

            // 全QUICコネクションにGOAWAYを送信
            {
                let conns = connections.borrow();
                let conn_count = conns.len();
                if conn_count > 0 {
                    info!("[HTTP/3] Sending GOAWAY to {} connections", conn_count);
                }
            }

            // コネクションが完了するまで待機（タイムアウト付き）
            let drain_timeout = Duration::from_secs(30);
            let drain_start = std::time::Instant::now();

            loop {
                let active_count = connections.borrow().len();
                if active_count == 0 {
                    info!("[HTTP/3] All connections drained");
                    break;
                }

                if drain_start.elapsed() > drain_timeout {
                    warn!(
                        "[HTTP/3] Drain timeout, {} connections still active",
                        active_count
                    );
                    break;
                }

                // タイムアウト処理を継続
                {
                    let mut conns = connections.borrow_mut();
                    let mut closed = Vec::new();
                    for (cid, handler) in conns.iter_mut() {
                        handler.conn.on_timeout();
                        if handler.conn.is_closed() {
                            closed.push(cid.clone());
                        }
                    }
                    for cid in closed {
                        conns.remove(&cid);
                    }
                }

                crate::runtime::time::sleep(Duration::from_millis(100)).await;
            }

            info!("[HTTP/3] Shutdown complete");
            break Ok(());
        }

        // 最小タイムアウトを計算
        let timeout_duration = {
            let conns = connections.borrow();
            conns
                .values()
                .filter_map(|h| h.conn.timeout())
                .min()
                .unwrap_or(Duration::from_millis(100))
        };

        // パケット受信・バックエンドタスク通知・タイムアウトの 3 者を多重化（F-32 + F-33）。
        // recv_gro_async は recvmsg(2) + UDP_GRO CMSG で同一フローの複数データグラムを
        // カーネルで集約受信し、per-datagram の syscall を削減する。GRO 非対応カーネルでは
        // 単発データグラムとして安全にフォールバックする（cmsg 無し → セグメント分割なし）。
        // バッファは loop 外で再利用するため、データグラム毎の 64KB ヒープ確保を排除。
        // io_uring の新規オペコードは増やさず、EAGAIN 時は POLL_ADD（wait_readable_fd）で待機。
        // F-32: バックエンドタスクがレスポンス断片を生成 or リクエストボディを消化したら
        // notify でメインループを起こし、低遅延でストリーミングを駆動する（負け arm の
        // recv_gro_async drop は既存 timeout と同じく cancel-safe）。
        let recv_outcome = futures::select_biased! {
            r = futures::FutureExt::fuse(socket.recv_gro_async(&mut recv_buf)) => RecvOutcome::Packet(r),
            _ = futures::FutureExt::fuse(notify.wait()) => RecvOutcome::Notified,
            _ = futures::FutureExt::fuse(crate::runtime::time::sleep(timeout_duration)) => RecvOutcome::Timeout,
        };

        // タイムアウト処理（常に実行）
        {
            let mut conns = connections.borrow_mut();
            let mut closed = Vec::new();
            for (cid, handler) in conns.iter_mut() {
                handler.conn.on_timeout();
                if handler.conn.is_closed() {
                    closed.push(cid.clone());
                }
            }
            for cid in closed {
                debug!("[HTTP/3] Connection closed (timeout)");
                conns.remove(&cid);
            }
        }

        // パケット受信結果を処理（通知/タイムアウト時も以降の drive + 送信処理は実行する）
        let gro_result = match recv_outcome {
            RecvOutcome::Packet(Ok(r)) => Some(r),
            RecvOutcome::Packet(Err(e)) => {
                if e.kind() != io::ErrorKind::WouldBlock {
                    error!("[HTTP/3] recv_gro error: {}", e);
                }
                None
            }
            // 通知 or タイムアウト - パケット受信なし、drive + 送信処理は続行
            RecvOutcome::Notified | RecvOutcome::Timeout => None,
        };

        // パケットを受信した場合のみ処理
        if let Some(gro) = gro_result {
            let from = gro.from;
            let total = gro.bytes_received;
            // GRO セグメントサイズ。None/0（GRO 非適用 = 単発データグラム）の場合は
            // 受信全体を 1 セグメントとして扱う。
            let seg_size = gro
                .gro_segment_size
                .map(|s| s as usize)
                .filter(|&s| s > 0)
                .unwrap_or(total);

            // GRO で集約された各 QUIC データグラムを quiche に供給する。
            // recv_buf のスライスを quiche::Header::from_slice と conn.recv に直接渡し、
            // 中間 Vec への 2 回の to_vec コピーを完全に排除する（ゼロコピー受信）。
            //
            // F-45: GRO バッチはカーネルが**同一フロー**のデータグラムを集約したものなので、
            // - `connections` の RefCell 借用はバッチ全体で 1 回だけ取る（従来はセグメント
            //   ごとに 2 回 borrow_mut していた）。
            // - 直前セグメントと同じ DCID なら新規接続判定（contains_key + Initial 検査）を
            //   スキップし、per-segment のオーバーヘッドをルックアップ 1 回に抑える。
            // quiche の `recv` API は 1 データグラム単位のため呼び出し自体は per-segment。
            let mut conns = connections.borrow_mut();
            let mut prev_cid: Option<ConnectionId<'static>> = None;
            let mut offset = 0;
            while offset < total {
                let start = offset;
                let end = (offset + seg_size).min(total);
                offset = end;

                // パケットヘッダーを解析（同一バッファスライスを後段の conn.recv にも渡す）
                let hdr = match quiche::Header::from_slice(
                    &mut recv_buf[start..end],
                    quiche::MAX_CONN_ID_LEN,
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("[HTTP/3] Invalid packet header: {}", e);
                        // このセグメントのみスキップ。送信処理はループ後に実行。
                        continue;
                    }
                };

                // コネクションを検索または作成（直前セグメントと同一 DCID なら判定スキップ）
                let conn_id = match &prev_cid {
                    Some(prev) if *prev == hdr.dcid => prev.clone(),
                    _ => {
                        if !conns.contains_key(&hdr.dcid) {
                            if hdr.ty != quiche::Type::Initial {
                                debug!("[HTTP/3] Non-initial packet for unknown connection");
                                continue;
                            }

                            // 新規コネクション
                            let mut scid = [0u8; quiche::MAX_CONN_ID_LEN];
                            rng.fill(&mut scid)
                                .map_err(|_| io::Error::other("RNG error"))?;
                            let scid = ConnectionId::from_ref(&scid).into_owned();

                            let mut config_ref = quic_config.borrow_mut();
                            let conn =
                                quiche::accept(&scid, None, local_addr, from, &mut config_ref)
                                    .map_err(|e| io::Error::other(e.to_string()))?;

                            debug!("[HTTP/3] New connection from {}", from);

                            // 最新のルーティング設定を取得
                            let handler = Http3Handler::new(
                                conn,
                                from,
                                notify.clone(),
                                backend_spawner.clone(),
                            );
                            conns.insert(scid.clone(), handler);

                            prev_cid = Some(scid.clone());
                            scid
                        } else {
                            let cid = hdr.dcid.into_owned();
                            prev_cid = Some(cid.clone());
                            cid
                        }
                    }
                };

                // パケットを処理（同一スライスをそのまま渡す。追加コピーなし）
                if let Some(handler) = conns.get_mut(&conn_id) {
                    let recv_info = quiche::RecvInfo {
                        from,
                        to: local_addr,
                    };

                    match handler.conn.recv(&mut recv_buf[start..end], recv_info) {
                        Ok(_) => {}
                        Err(e) => {
                            warn!("[HTTP/3] recv error: {}", e);
                            // エラー時も送信処理は続行
                        }
                    }
                }
            }
            drop(conns);

            // ハンドシェイクパケット送信（H3初期化前に送信することでCryptoFail回避）
            // recv()後、is_established()がtrueになる前にServer Helloを送信する必要がある
            send_pending_packets(&connections, &socket, local_addr).await;
        }

        // H3初期化とイベント処理（B-12: パケット受信時だけでなく**毎イテレーション**実行する）。
        //
        // `drive_proxy_streams` の `recv_body`（`h3.poll()` の外）がストリームを進めると、
        // h3 イベントは poll でしか取り出せない形で滞留する。具体例（B-12 のハング）:
        // h3 クライアント（hyperium h3）は fin 直前に GREASE フレームを送るため、pump の
        // `recv_body` が最終 DATA を消費してもフレームペイロードが未読で残り
        // （非 DATA フレームの消費は poll 専用）、`Finished` も生成されない。クライアントは
        // 送信完了後は無通信のため「パケット到着時のみ poll」だと永久に取り残され、
        // EOF 未伝播 → レスポンス無し → QUIC アイドルタイムアウトの双方向デッドロックに陥る。
        // poll はイベントが無ければ即 `Done` を返すだけで安価なので、毎イテレーション呼ぶ。
        {
            let mut conns = connections.borrow_mut();
            for (_, handler) in conns.iter_mut() {
                // HTTP/3 初期化
                if handler.h3_conn.is_none() && handler.conn.is_established() {
                    debug!("[HTTP/3] Connection established, initializing H3");
                }
                if let Err(e) = handler.init_h3() {
                    warn!("[HTTP/3] init_h3 error: {}", e);
                }

                // 書き込み可能なストリームを処理（quiche パターン）
                // 部分レスポンスを再送する
                if handler.h3_conn.is_some() {
                    if let Err(e) = handler.handle_writable_streams() {
                        warn!("[HTTP/3] handle_writable_streams error: {}", e);
                    }
                }

                // HTTP/3 イベント処理
                if handler.h3_conn.is_some() {
                    if let Err(e) = handler.process_h3_events().await {
                        warn!("[HTTP/3] process_h3_events error: {}", e);
                    }
                }
            }
        }

        // F-32: 全ハンドラのストリーミングストリームを駆動（パケット無し = 通知/タイムアウト時も）。
        // バックエンドタスクが生成したレスポンス断片を send_body、recv_body したリクエストボディを
        // チャネルへ流す。フロー制御でブロックした分は次イテレーションで再試行される。
        {
            let mut conns = connections.borrow_mut();
            for (_, handler) in conns.iter_mut() {
                if handler.h3_conn.is_some() {
                    handler.drive_proxy_streams();
                }
            }
        }

        // 送信処理（常に実行 - タイムアウト時も送信が必要）
        send_pending_packets(&connections, &socket, local_addr).await;

        // F-44: 協調的 yield。パケットが連続して到着すると select の recv arm が
        // 即 Ready になり続け、本タスクが単一 poll 内でループし続けて同一スレッドの
        // バックエンド I/O タスク（TLS ハンドシェイク・TCP 転送）が飢餓する。
        // 毎イテレーション一度キュー末尾へ譲り、spawn 済みタスクを 1 巡実行させる。
        crate::runtime::yield_now().await;
    }
}

/// 保留中のパケットを全コネクションに対して送信
///
/// この関数はメインループで常に呼び出され、タイムアウト時でも
/// ACKやレスポンスパケットを送信します。
// clippy::await_holding_refcell_ref 許容理由: `connections`（Rc<RefCell<HashMap>>）を
// 借用するのは本 H3 メインループタスクのみ。バックエンドタスクは Rc チャネル + Notify
// 経由で通信し RefCell に触れない（F-32 のアクターモデル）ため、await 中に他タスクが
// 再入借用して panic する経路は存在しない（B-16 とは異なり単一借用者）。
#[allow(clippy::await_holding_refcell_ref)]
async fn send_pending_packets(
    connections: &ConnectionMap,
    socket: &Rc<QuicUdpSocket>,
    _local_addr: SocketAddr,
) {
    let mut conns = connections.borrow_mut();

    // 送信用スクラッチ（send_buf 1350B + GSO 連結バッファ + パケット境界）を
    // スレッドローカルから払い出して再利用する。thread-per-core のためロック不要。
    // take/replace により .await をまたいでスレッドローカルの borrow を保持しないので、
    // 再入（このループ内での多重呼び出し）でも安全。これにより送信のたびに発生していた
    // 1350B + バッチの malloc を排除する。
    let mut scratch = take_h3_send_scratch();
    let H3SendScratch {
        send_buf,
        batch,
        offsets,
    } = &mut scratch;
    let mut closed = Vec::new();

    for (cid, handler) in conns.iter_mut() {
        batch.clear();
        offsets.clear();
        let mut seg_size = 0usize;
        let mut dest: Option<SocketAddr> = None;

        // F-60: GSO セグメントサイズの自動調整。quiche の PMTU 探索結果
        // （`max_send_udp_payload_size`: ハンドシェイク中 1200 → 検証後は
        // 設定上限・経路 MTU の小さい方へ成長）に per-connection で追従し、
        // 下限 MIN_UDP_SEND_PAYLOAD / 上限 send_buf 長でクランプする。
        // これによりハンドシェイク初期は RFC 準拠の 1200B、検証後は経路が許す
        // 最大セグメントで GSO バッチが構成される。
        let max_payload = handler
            .conn
            .max_send_udp_payload_size()
            .clamp(MIN_UDP_SEND_PAYLOAD, send_buf.len());

        loop {
            let (write, send_info) = match handler.conn.send(&mut send_buf[..max_payload]) {
                Ok(v) => v,
                Err(quiche::Error::Done) => break,
                Err(quiche::Error::CryptoFail) => {
                    // ハンドシェイク途中のため暗号化パケット生成に失敗
                    // 次のイテレーションで再試行される（コネクションは閉じない）
                    debug!("[HTTP/3] CryptoFail (handshake in progress), will retry");
                    break;
                }
                Err(e) => {
                    error!("[HTTP/3] send error: {}", e);
                    handler.conn.close(false, 0x1, b"send error").ok();
                    break;
                }
            };

            // 宛先が変わった or セグメントサイズが変わった（均一バッチの境界）場合は
            // 現在のバッチを先に flush する（GSO は最終セグメント以外を均一サイズ要求）。
            // B-18: 合計バイトが sendmsg の UDP ペイロード上限を超える場合も先に flush する
            // （超過すると EMSGSIZE でバッチ全体が破棄される）。
            let dest_changed = dest.is_some_and(|d| d != send_info.to);
            if dest_changed
                || gso_batch_must_flush_before_append(offsets.len(), batch.len(), write, seg_size)
            {
                if let Some(d) = dest {
                    flush_gso_batch(socket, batch.as_slice(), offsets.as_slice(), d).await;
                }
                batch.clear();
                offsets.clear();
                seg_size = 0;
            }

            if offsets.is_empty() {
                seg_size = write;
            }
            let start = batch.len();
            batch.extend_from_slice(&send_buf[..write]);
            offsets.push((start, write));
            dest = Some(send_info.to);

            // バッチ満杯（GSO セグメント上限）or 最終セグメント（< seg_size）なら flush。
            if offsets.len() >= MAX_GSO_SEGMENTS || write < seg_size {
                flush_gso_batch(socket, batch.as_slice(), offsets.as_slice(), send_info.to).await;
                batch.clear();
                offsets.clear();
                seg_size = 0;
                dest = None;
            }
        }

        // 残りのバッチを flush
        if !offsets.is_empty() {
            if let Some(d) = dest {
                flush_gso_batch(socket, batch.as_slice(), offsets.as_slice(), d).await;
            }
        }

        if handler.conn.is_closed() {
            debug!("[HTTP/3] Connection closed from {}", handler.peer_addr);
            closed.push(cid.clone());
        }
    }

    for cid in closed {
        conns.remove(&cid);
    }

    // スクラッチをスレッドローカルへ返却し、次回呼び出しで再利用する（malloc 排除）。
    put_h3_send_scratch(scratch);
}

/// GSO セグメント上限（UDP GSO の一般的な最大セグメント数）
const MAX_GSO_SEGMENTS: usize = 64;

/// F-60: 送信セグメントサイズの下限クランプ（RFC 9000 の最小 QUIC データグラム 1200B）
const MIN_UDP_SEND_PAYLOAD: usize = 1200;

/// F-60: 送信セグメントサイズの上限クランプ（単一 UDP データグラムの最大ペイロード。
/// 65535 - 8(UDP ヘッダ) - 20(IPv4 ヘッダ) = 65507）。
/// 実際のセグメントサイズは quiche の PMTU 探索と設定 `max_udp_payload_size` の
/// 小さい方に per-connection で自動追従する（`send_pending_packets` 参照）。
const MAX_UDP_SEND_PAYLOAD: usize = 65507;

/// B-18: 1 回の sendmsg(UDP_SEGMENT) に載せられる GSO バッチ合計バイト上限。
/// UDP sendmsg のペイロード上限（65507）を超えると EMSGSIZE でバッチ全体が破棄される
/// （QUIC の再送で回復するが帯域・レイテンシを浪費する）ため、超過前に flush する。
/// 従来は MAX_GSO_SEGMENTS(64) × 1350B = 86.4KB まで蓄積し得たため上限超過が起こり得た。
const MAX_GSO_BATCH_BYTES: usize = 65507;

/// 次パケット（`write` バイト）をバッチへ追加する**前に** flush が必要か判定する。
///
/// - 均一サイズ要求: バッチ内の既存セグメントサイズ `seg_size` と異なるサイズは同居不可
///   （GSO は最終セグメントのみ小さくてよい。大きくなるケースは分割が必要）
/// - B-18: 追加すると合計が `MAX_GSO_BATCH_BYTES` を超える場合は先に flush
#[inline]
fn gso_batch_must_flush_before_append(
    offsets_len: usize,
    batch_len: usize,
    write: usize,
    seg_size: usize,
) -> bool {
    if offsets_len == 0 {
        return false;
    }
    write != seg_size || batch_len + write > MAX_GSO_BATCH_BYTES
}

/// `send_pending_packets` 用の送信スクラッチ（スレッドローカルで再利用）。
struct H3SendScratch {
    /// quiche の単一パケット書き出し用バッファ（F-60: 上限クランプ長で確保し、
    /// per-connection の動的セグメントサイズでスライスして使用）
    send_buf: Vec<u8>,
    /// GSO バッチ連結バッファ
    batch: Vec<u8>,
    /// バッチ内のパケット境界 (offset, len)
    offsets: Vec<(usize, usize)>,
}

thread_local! {
    /// 送信スクラッチのスレッドローカル保管庫。thread-per-core のためロック不要。
    static H3_SEND_SCRATCH: std::cell::RefCell<Option<H3SendScratch>> =
        const { std::cell::RefCell::new(None) };
}

/// スクラッチを払い出す（無ければ新規確保）。take してから返すため、.await をまたいで
/// スレッドローカルの borrow を保持しない（再入安全）。
fn take_h3_send_scratch() -> H3SendScratch {
    H3_SEND_SCRATCH
        .with(|s| s.borrow_mut().take())
        .unwrap_or_else(|| H3SendScratch {
            send_buf: vec![0u8; MAX_UDP_SEND_PAYLOAD],
            batch: Vec::new(),
            offsets: Vec::new(),
        })
}

/// スクラッチを返却する（次回再利用）。肥大化した batch は一定上限で解放してメモリを抑える。
fn put_h3_send_scratch(mut scratch: H3SendScratch) {
    scratch.batch.clear();
    scratch.offsets.clear();
    // バッチが極端に肥大化した場合（>1MB）は確保を手放す。
    if scratch.batch.capacity() > (1 << 20) {
        scratch.batch.shrink_to(64 * 1500);
    }
    H3_SEND_SCRATCH.with(|s| *s.borrow_mut() = Some(scratch));
}

/// 連結済みバッチ（`offsets` でパケット境界を持つ）を送出する。
///
/// 単一パケットは通常送信、複数パケットは `send_gso_async`（GSO 無効時は個別送信へ
/// 安全にフォールバック）で 1 回の sendmsg(UDP_SEGMENT) にまとめて送る。
async fn flush_gso_batch(
    socket: &Rc<QuicUdpSocket>,
    batch: &[u8],
    offsets: &[(usize, usize)],
    dest: SocketAddr,
) {
    match offsets {
        [] => {}
        [(start, len)] => {
            // 単一パケット: ゼロアロケーション送信（to_vec 不要）。
            // batch スライスを直接 sendto するため、パケットごとのヒープ確保を排除。
            if let Err(e) = socket
                .send_to_slice_async(&batch[*start..*start + *len], dest)
                .await
            {
                warn!("[HTTP/3] send_to error: {}", e);
            }
        }
        _ => {
            // 複数パケット: GSO（send_gso_async は GSO 無効環境では個別送信に
            // フォールバックするため安全）。スライスはバッチへの参照でアロケーションは
            // パケット境界ベクタ 1 本のみ。
            let packets: Vec<&[u8]> = offsets
                .iter()
                .map(|&(start, len)| &batch[start..start + len])
                .collect();
            if let Err(e) = socket.send_gso_async(&packets, dest).await {
                warn!("[HTTP/3] GSO batch send error: {}", e);
            }
        }
    }
}

/// HTTP/3 サーバーを起動（同期ラッパー）
///
/// 別スレッドで monoio ランタイムを作成して実行します。
pub fn run_http3_server(bind_addr: SocketAddr, config: Http3ServerConfig) -> io::Result<()> {
    // カスタム io_uring ランタイムで非同期 HTTP/3 サーバーを実行
    crate::runtime::block_on(async move { run_http3_server_async(bind_addr, config).await })
}

// ====================
// ヘルパー関数
// ====================

/// HTTP/3 用レスポンスボディ圧縮ヘルパー関数
///
/// バイト配列を受け取り、指定されたエンコーディングで圧縮して返します。
/// 圧縮に失敗した場合は元のデータをそのまま返します。
#[cfg(feature = "compression")]
pub(crate) fn compress_body_h3(
    body: &[u8],
    encoding: AcceptedEncoding,
    compression: &CompressionConfig,
) -> Vec<u8> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    match encoding {
        AcceptedEncoding::Zstd => {
            match zstd::encode_all(std::io::Cursor::new(body), compression.zstd_level) {
                Ok(compressed) => compressed,
                Err(_) => body.to_vec(),
            }
        }
        AcceptedEncoding::Gzip => {
            let level = Compression::new(compression.gzip_level);
            let mut encoder = GzEncoder::new(Vec::with_capacity(body.len()), level);
            if encoder.write_all(body).is_err() {
                return body.to_vec();
            }
            encoder.finish().unwrap_or_else(|_| body.to_vec())
        }
        AcceptedEncoding::Brotli => {
            let mut compressed = Vec::with_capacity(body.len());
            let params = brotli::enc::BrotliEncoderParams {
                quality: compression.brotli_level as i32,
                ..Default::default()
            };
            let mut input = std::io::Cursor::new(body);
            if brotli::BrotliCompress(&mut input, &mut compressed, &params).is_err() {
                return body.to_vec();
            }
            compressed
        }
        AcceptedEncoding::Deflate => {
            use flate2::write::DeflateEncoder;
            let level = Compression::new(compression.gzip_level);
            let mut encoder = DeflateEncoder::new(Vec::with_capacity(body.len()), level);
            if encoder.write_all(body).is_err() {
                return body.to_vec();
            }
            encoder.finish().unwrap_or_else(|_| body.to_vec())
        }
        AcceptedEncoding::Identity => body.to_vec(),
    }
}

/// compression feature 無効時のスタブ
#[cfg(not(feature = "compression"))]
#[inline]
pub(crate) fn compress_body_h3(
    body: &[u8],
    _encoding: AcceptedEncoding,
    _compression: &CompressionConfig,
) -> Vec<u8> {
    body.to_vec()
}

/// HTTPレスポンスのヘッダー終端（\r\n\r\n）を探す
fn find_header_end(data: &[u8]) -> Option<usize> {
    for i in 0..data.len().saturating_sub(3) {
        if &data[i..i + 4] == b"\r\n\r\n" {
            return Some(i);
        }
    }
    None
}

/// HTTPレスポンスからステータスコードをパース
fn parse_status_code(header: &[u8]) -> Option<u16> {
    // "HTTP/1.1 200 OK" のような形式
    let header_str = std::str::from_utf8(header).ok()?;
    let first_line = header_str.lines().next()?;
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    if parts.len() >= 2 {
        parts[1].parse().ok()
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default() {
        let config = Http3ServerConfig::default();
        assert_eq!(config.max_idle_timeout, 30000);
        assert_eq!(config.max_udp_payload_size, 1350);
    }

    /// B-18: GSO バッチの flush 判定。
    #[test]
    fn test_gso_batch_flush_rules() {
        // 空バッチには常に追加可能（flush 不要）
        assert!(!gso_batch_must_flush_before_append(0, 0, 1350, 0));
        assert!(!gso_batch_must_flush_before_append(0, 0, 65507, 0));

        // 均一サイズ・上限内は追加可能
        assert!(!gso_batch_must_flush_before_append(2, 2700, 1350, 1350));

        // セグメントサイズが変わる場合は flush（GSO の均一サイズ要求）
        assert!(gso_batch_must_flush_before_append(2, 2700, 800, 1350));
        assert!(gso_batch_must_flush_before_append(2, 2700, 1500, 1350));

        // B-18: 合計バイトが sendmsg の UDP ペイロード上限を超える場合は flush。
        // 従来は 64 セグメント × 1350B = 86.4KB まで蓄積し EMSGSIZE でバッチ全体が
        // 破棄されていた（48 セグメント目で 64800 + 1350 > 65507）。
        assert!(gso_batch_must_flush_before_append(
            48,
            48 * 1350,
            1350,
            1350
        ));

        // ちょうど上限までは許容
        let seg = 1000;
        let batch_len = 64_507; // + 1000 = 65507 (== MAX_GSO_BATCH_BYTES)
        assert!(!gso_batch_must_flush_before_append(10, batch_len, seg, seg));
        assert!(gso_batch_must_flush_before_append(
            10,
            batch_len + 1,
            seg,
            seg
        ));
    }
}
