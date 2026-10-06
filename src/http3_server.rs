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

// AsRawFd は memfd 経由の証明書リロード（Linux）と、Linux + io_uring の
// UDP パイプライン（`PipelinedUdpRecv` / `UringUdpSend`）でのみ使用する。
// 非 Linux（FreeBSD/OpenBSD/NetBSD/macOS/Windows）では未使用になるため cfg で絞る
// （unused_imports 警告対策。F-136 で非 Linux は in-memory SSL_CTX 経路へ移った）。
use crate::cache;
#[cfg(target_os = "linux")]
use crate::runtime::handle::AsRawFd;
use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
// CString / FromRawFd / Seek / Write は memfd 経由の証明書リロード（Linux 専用）でのみ
// 使用する。
#[cfg(target_os = "linux")]
use std::ffi::CString;
use std::io::{self};
#[cfg(target_os = "linux")]
use std::io::{Seek, Write as IoWrite};
use std::net::SocketAddr;
#[cfg(target_os = "linux")]
use std::os::unix::io::FromRawFd;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Once;
use std::time::{Duration, Instant};

use crate::udp::QuicUdpSocket;
// F-122: RNG は OpenBSD では ring、他は aws-lc-rs（crate::tls_provider で選択）。
use crate::tls_provider::{SecureRandom, SystemRandom};
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
use crate::metrics::{
    encode_prometheus_metrics, http3_stream_closed, http3_stream_opened, http3_streams_closed_n,
    Http3ActiveConnGuard,
};
use crate::pool::MAX_HEADER_SIZE;
use crate::proxy::{check_security, SecurityCheckResult};
use crate::upstream::find_backend_unified;

/// HTTP/3 リクエストヘッダブロックの近似サイズ（name + value の合計）。
///
/// H1 のワイヤ上ヘッダサイズ制限（`MAX_HEADER_SIZE`）と同等の DoS 防御に使う。
/// QPACK 展開後の論理サイズで判定する（圧縮効率に依存しない）。
fn h3_request_header_block_size(headers: &[h3::Header]) -> usize {
    headers
        .iter()
        .map(|h| h.name().len().saturating_add(h.value().len()))
        .sum()
}

/// memfd_create システムコールのラッパー（セキュリティ強化版）
///
/// 匿名のメモリファイルを作成します。このファイルはファイルシステム上には
/// 存在せず、メモリ上にのみ存在します。Landlock のファイルシステム制限を
/// バイパスしながら、ファイルディスクリプタ経由でアクセスできます。
///
/// ## セキュリティ対策
/// - MFD_CLOEXEC: exec() 時に自動的に閉じる（fd リーク防止）
/// - MFD_ALLOW_SEALING: 書き込み後にシールを適用可能にする
///
/// `memfd_create(2)` は Linux にある。非 Linux（FreeBSD/OpenBSD/NetBSD/macOS/Windows）は
/// quiche の in-memory `SSL_CTX` API（`with_boring_ssl_ctx_builder`）を使うため、
/// memfd 自体が不要（F-136）。本関数は Linux のみでコンパイルする。
#[cfg(target_os = "linux")]
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
///
/// memfd を持つ Linux でのみコンパイルする（F-136）。
#[cfg(target_os = "linux")]
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

/// PEM データを memfd に書き込み、quiche へ渡すパスを返す（セキュリティ強化版）
///
/// Linux 専用: `/proc/self/fd/<fd>` 経由・FS 非経由。非 Linux（FreeBSD/OpenBSD/NetBSD/
/// macOS/Windows）は quiche の in-memory `SSL_CTX` API（`with_boring_ssl_ctx_builder`）を
/// 使うため、本関数自体が不要（`new_quic_config_with_certs`/`reload_quiche_certs` の
/// `#[cfg(not(target_os = "linux"))]` 実装を参照、F-136）。
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
#[cfg(target_os = "linux")]
struct PemBackedFile {
    _file: std::fs::File,
}

#[cfg(target_os = "linux")]
fn create_memfd_for_pem(name: &str, pem_data: &[u8]) -> io::Result<(PemBackedFile, String)> {
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

    Ok((PemBackedFile { _file: memfd }, proc_path))
}

/// PEM バイト列から証明書・秘密鍵を設定した新規 `quiche::Config` を構築する（F-136）。
///
/// - Linux: `create_memfd_for_pem`（`/proc/self/fd/<fd>` 経由・FS 非経由）で
///   一時的にファイル化し、`load_cert_chain_from_pem_file`/`load_priv_key_from_pem_file`
///   （パス指定 API、quiche の TLS バックエンドに依存しない汎用 API）へパスとして渡す。
///   **Linux 経路（`target_os = "linux"`）は F-136 で 1 行も変更しない**
///   （AGENTS.md「Linux 経路は不変」）。
/// - 上記以外（FreeBSD/OpenBSD/NetBSD/macOS/Windows）: PEM バイト列から
///   直接 BoringSSL の `SslContextBuilder` を組み、`Config::with_boring_ssl_ctx_builder` で
///   ロードする。ファイル・パス・memfd を一切介さないため、FreeBSD capsicum capability
///   mode / OpenBSD pledge+unveil のいずれの下でも動作する（設計根拠は
///   docs/artifacts/f136_platform_design.md の F-136 節参照）。
#[cfg(target_os = "linux")]
fn new_quic_config_with_certs(cert_pem: &[u8], key_pem: &[u8]) -> io::Result<Config> {
    let mut quic_config = Config::new(quiche::PROTOCOL_VERSION)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;

    let (cert_memfd, cert_path) = create_memfd_for_pem("tls_cert", cert_pem)
        .map_err(|e| io::Error::other(format!("Failed to create memfd for cert: {}", e)))?;
    quic_config
        .load_cert_chain_from_pem_file(&cert_path)
        .map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("cert load error (memfd): {}", e),
            )
        })?;
    drop(cert_memfd);

    let (key_memfd, key_path) = create_memfd_for_pem("tls_key", key_pem)
        .map_err(|e| io::Error::other(format!("Failed to create memfd for key: {}", e)))?;
    quic_config
        .load_priv_key_from_pem_file(&key_path)
        .map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("key load error (memfd): {}", e),
            )
        })?;
    drop(key_memfd);

    Ok(quic_config)
}

/// PEM バイト列から証明書・秘密鍵を設定した新規 `quiche::Config` を構築する
/// （F-136、Linux 以外）。
///
/// `new_quic_config_with_certs` の doc コメント参照。`boringssl-boring-crate` feature
/// （外部 `boring` crate）でのみ提供される in-memory SSL_CTX API を使う。
#[cfg(not(target_os = "linux"))]
fn new_quic_config_with_certs(cert_pem: &[u8], key_pem: &[u8]) -> io::Result<Config> {
    use boring::pkey::PKey;
    use boring::ssl::{SslContextBuilder, SslMethod};
    use boring::x509::X509;

    let mut builder = SslContextBuilder::new(SslMethod::tls()).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("boring SslContextBuilder::new: {}", e),
        )
    })?;

    let mut chain = X509::stack_from_pem(cert_pem)
        .map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("certificate parse error: {}", e),
            )
        })?
        .into_iter();
    let leaf = chain
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty certificate chain"))?;
    builder.set_certificate(&leaf).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("set_certificate: {}", e),
        )
    })?;
    // 残りは中間 CA チェーン。
    for extra in chain {
        builder.add_extra_chain_cert(extra).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("add_extra_chain_cert: {}", e),
            )
        })?;
    }

    let pkey = PKey::private_key_from_pem(key_pem).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("private key parse error: {}", e),
        )
    })?;
    builder.set_private_key(&pkey).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("set_private_key: {}", e),
        )
    })?;

    Config::with_boring_ssl_ctx_builder(quiche::PROTOCOL_VERSION, builder)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))
}

/// QUIC トランスポートパラメータを `Config` へ適用する（TLS 証明書設定とは独立、初回ロード・
/// リロード共通。F-136 でリロード時にも呼べるよう独立関数へ切り出した）。
fn configure_quic_transport(
    quic_config: &mut Config,
    config: &Http3ServerConfig,
) -> io::Result<()> {
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

    // F-124: 輻輳制御 / Pacing / HyStart++（quiche 低レベル Config API）
    let cc_name = config.cc_algorithm.trim();
    if let Err(e) = quic_config.set_cc_algorithm_name(cc_name) {
        warn!(
            "[HTTP/3] unknown cc_algorithm '{}': {}; falling back to bbr",
            cc_name, e
        );
        let _ = quic_config.set_cc_algorithm_name("bbr");
    }
    quic_config.enable_pacing(config.pacing);
    if let Some(rate) = config.max_pacing_rate {
        quic_config.set_max_pacing_rate(rate);
    }
    quic_config.enable_hystart(config.hystart);

    // HTTP/3 用の ALPN を設定
    quic_config
        .set_application_protos(h3::APPLICATION_PROTOCOL)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;

    Ok(())
}

/// 稼働中の `quiche::Config` の証明書・秘密鍵を差し替える（F-105 ホットリロード、F-136 で
/// capability mode 対応）。
///
/// - Linux: 既存 `Config` の内部 SSL_CTX に対して
///   `load_cert_chain_from_pem_file`/`load_priv_key_from_pem_file` を呼び直すだけで済む
///   （メモリ確保済みの他のトランスポートパラメータは無傷のまま）。**Linux 経路は無変更**。
/// - それ以外（非 Linux）: `with_boring_ssl_ctx_builder` は**新しい
///   `Config` を構築する** API であり、既存 `Config` の SSL_CTX だけを差し替えることは
///   できない。そのため `new_quic_config_with_certs` で新規 `Config` を作り、
///   `configure_quic_transport` で初回ロードと同じトランスポートパラメータを再適用した
///   うえで、`RefCell` の中身を丸ごと入れ替える（初回ロードとリロードで同じ in-memory
///   経路を通る）。
///
/// # ホットパス例外について（AGENTS.md）
/// 本処理はパース + memfd 書き込み/BoringSSL 初期化で数 ms ループをブロックするが、証明書更新は
/// 数ヶ月に 1 回の**コールドパス**であり、イベントループ先頭の世代ゲートで差分検知時のみ実行
/// される。ホットパス絶対規則の明示的な例外として許容する（既存接続は `quiche::accept` 時に
/// SSL_CTX から複製済みのため影響を受けず、以後の新規ハンドシェイクのみ新証明書を提示する）。
fn reload_quiche_certs(
    quic_config: &Rc<RefCell<Config>>,
    material: &crate::tls_reload::Http3CertMaterial,
    // Linux では未使用（既存 Config の SSL_CTX だけを差し替えるため）。
    // アンダースコア接頭辞はその場合の unused 警告抑制であり、それ以外では通常どおり使用する。
    _transport_config: &Http3ServerConfig,
) -> io::Result<()> {
    material.load_into(|cert_pem, key_pem| {
        #[cfg(target_os = "linux")]
        {
            let mut cfg = quic_config.borrow_mut();

            // 証明書チェーンを memfd 経由で差し替え。
            let (cert_memfd, cert_path) = create_memfd_for_pem("tls_cert_reload", cert_pem)?;
            cfg.load_cert_chain_from_pem_file(&cert_path).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("cert reload error (memfd): {}", e),
                )
            })?;
            // memfd はロード完了後ただちにクローズ（機密の滞留を避ける）。
            drop(cert_memfd);

            // 秘密鍵を memfd 経由で差し替え。
            let (key_memfd, key_path) = create_memfd_for_pem("tls_key_reload", key_pem)?;
            cfg.load_priv_key_from_pem_file(&key_path).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("key reload error (memfd): {}", e),
                )
            })?;
            drop(key_memfd);

            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            // F-136: capability mode / pledge+unveil 下でも動作する in-memory 経路。
            // 初回ロードと全く同じ手順で新規 Config を構築し、まるごと差し替える。
            let mut new_cfg = new_quic_config_with_certs(cert_pem, key_pem)?;
            configure_quic_transport(&mut new_cfg, _transport_config)?;
            *quic_config.borrow_mut() = new_cfg;
            Ok(())
        }
    })
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
    /// QUIC 輻輳制御アルゴリズム名（quiche `set_cc_algorithm_name`）。デフォルト: `"bbr"`
    pub cc_algorithm: String,
    /// Packet Pacing を有効にするか（quiche `enable_pacing`）。デフォルト: true
    pub pacing: bool,
    /// 最大 pacing レート（バイト/秒）。`None` は制限なし
    pub max_pacing_rate: Option<u64>,
    /// HyStart++ を有効にするか（quiche `enable_hystart`）。デフォルト: true
    pub hystart: bool,
    /// UDP mmsg / multishot バッチ幅（1..=128）。デフォルト: 64
    pub mmsg_batch_size: usize,
    /// reactor バックエンド専用: 1 イテレーションあたりに掻き出す UDP データグラム数の上限
    /// （`[http3] recv_drain_max`、1..=`H3_RECV_DRAIN_MAX_LIMIT`）。デフォルト: 64（F-152）
    pub recv_drain_max: usize,
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
            cc_algorithm: "bbr".to_string(),
            pacing: true,
            max_pacing_rate: None,
            hystart: true,
            mmsg_batch_size: crate::udp::socket::MMSG_BATCH_DEFAULT,
            recv_drain_max: H3_RECV_DRAIN_MAX_DEFAULT,
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

/// B-43: StreamBlocked で保留した静的応答。head が Some の間はヘッダ未送出。
///
/// 従来は `(body, written)` のみを保存しており、HEADERS が StreamBlocked に
/// なった際にヘッダ未送出のままボディだけ保存 → 再送で `send_body()` を先に
/// 呼び quiche h3 が `FrameUnexpected` を返す不具合があった。head を持たせ、
/// 再送側でヘッダ→ボディの順序を守れるようにする。
struct PartialResponse {
    /// 未送出のレスポンスヘッダ。Some の間はまだ HEADERS を送っていない。
    head: Option<Vec<h3::Header>>,
    /// ボディ。空 = ヘッダのみ応答（リダイレクト等）。
    body: Vec<u8>,
    /// 送信済みボディバイト数。
    written: usize,
}

/// `Arc<Vec<u8>>` を `Bytes::from_owner` でゼロコピー化するための薄いラッパ（F-169）。
///
/// `Backend::MemoryFile` はコンテンツを `Arc<Vec<u8>>` で保持しており、`Arc<Vec<u8>>`
/// 自体は `AsRef<[u8]>` を実装しない（`AsRef<Vec<u8>>` のみ）ため、そのまま
/// `Bytes::from_owner` には渡せない。`src/proxy.rs` の同名ラッパと同じ設計。
struct ArcVecBytes(Arc<Vec<u8>>);

impl AsRef<[u8]> for ArcVecBytes {
    fn as_ref(&self) -> &[u8] {
        self.0.as_slice()
    }
}

/// `Http3Handler::handle_sendfile` の引数まとめ。
///
/// F-132: WASM モジュールリストを渡す引数が増えたことで `clippy::too_many_arguments`
/// に抵触するため、理由なし `#[allow]` を増やす代わりに構造体へまとめた。
struct SendFileRequest<'a> {
    stream_id: u64,
    base_path: &'a Path,
    is_dir: bool,
    index_file: Option<&'a str>,
    req_path: &'a [u8],
    prefix: &'a [u8],
    security: &'a SecurityConfig,
    /// 圧縮設定（F-169: HTTP/3 専用設定を解決済みのもの。h2 の `h2_sendfile` と同様、
    /// 呼び出し元が `resolve_http3_compression_config` で解決してから渡す）。
    compression: &'a CompressionConfig,
    /// クライアントの Accept-Encoding から解決した圧縮方式（F-169）。
    client_encoding: AcceptedEncoding,
    /// OpenFileCache 設定（ルーティングごとの上書き、F-146 で is_dir 判定にも使用）
    open_file_cache_config: Option<&'a cache::OpenFileCacheConfig>,
    /// base_path の canonical 形（F-145、config ロード時に一度だけ解決）。
    /// ディレクトリルートの per-route 封じ込め検査に使う（B-65 続き）。
    canonical_base: Option<&'a Path>,
    /// 静的コンテンツキャッシュ設定（F-146、ルーティングごとの上書き）
    static_file_cache_config: Option<&'a cache::StaticContentCacheRouteConfig>,
    #[cfg(feature = "wasm")]
    wasm_modules: Option<&'a Arc<Vec<crate::wasm_plugin_config::ModuleRef>>>,
}

/// QUIC ストリーム ID をキーにするマップ用の軽量ハッシャ（乗算ハッシュ 1 回）。
///
/// 既定の SipHash はストリームごとの挿入・検索・削除のたびに走り、HTTP/3 の小さい応答では
/// プロファイル上位に出ていた。ストリーム ID は QUIC の規約上ピアが任意に散らせない
/// （低い ID から順に開く必要があり、同時数は max_streams で制限される）ため、
/// HashDoS 耐性の強いハッシャは不要。
#[derive(Default, Clone, Copy)]
struct StreamIdHasher(u64);

impl std::hash::Hasher for StreamIdHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(8) ^ b as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    #[inline]
    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0 ^ n).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}

type StreamIdHasherBuilder = std::hash::BuildHasherDefault<StreamIdHasher>;
type StreamMap<V> = HashMap<u64, V, StreamIdHasherBuilder>;

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
    /// 部分的なレスポンス（ストリーム ID → 保留中の応答。B-43 で head を保持）
    partial_responses: StreamMap<PartialResponse>,
    /// クライアント IP アドレス（F-168 P3: `to_string()` のヒープ確保を排除するスタックバッファ）
    client_ip: crate::http_utils::IpStr,
    /// ストリーミングプロキシ中のストリーム（F-32）。
    proxy_streams: StreamMap<ProxyStream>,
    /// バッファ経路の保留リクエスト（F-32）。
    buffered_reqs: StreamMap<BufferedReq>,
    /// ストリームごとのリクエストボディ蓄積（バッファ経路 + ストリーミング初回バッチ）。
    stream_bodies: StreamMap<BytesMut>,
    /// バックエンドタスク → メインループの起床通知（F-32）。F-151 で per-connection 化
    /// （`ConnWaker`）し、`notify()` 前に自 cid を共有起床キューへ積むようにした。
    notify: crate::http3_stream::ConnWaker,
    /// バックエンドタスクのスポーナ（F-46: 型付きタスクプール。ワーカースレッドで共有）。
    backend_spawner: crate::http3_stream::BackendSpawner,
    /// F-99: QUIC 接続ゲージ（Drop で自動 dec。ホットパス無アロケーション）
    _conn_metric: Http3ActiveConnGuard,
    /// F-99: メトリクス計上中のリクエストストリーム ID（open/close の二重計上防止）
    metric_open_streams: HashSet<u64, StreamIdHasherBuilder>,
    /// F-151: ダーティ集合への多重登録防止フラグ。`true` の間はメインループの
    /// `dirty_queue` に既に自 cid が積まれている。
    dirty: bool,
    /// F-151: タイマーヒープへ最後に登録した期限。ヒープ pop 時にこの値と一致する
    /// エントリだけを有効とみなす（遅延削除。期限更新後の古いエントリは無視して捨てる）。
    timer_deadline: Option<Instant>,
    /// F-151（レビュー修正）: 自接続 ID の共有ハンドル。ダーティキュー/タイマーヒープ/
    /// 送出対象リストへ渡す際は `key.clone()`（`Rc::clone`、参照カウント +1 のみ）で済ませ、
    /// `ConnectionId` 本体（内部 `Vec<u8>`）のディープコピーをホットパスから排除する。
    key: crate::http3_stream::ConnKey,
    /// F-168 P3: `process_h3_events` の新規 Headers 一時収集バッファ（再利用）。
    /// リクエストごとの `Vec` 確保をなくすため、`process_h3_events` が `mem::take` で
    /// 借り出して使い、末尾で `clear()` してから本フィールドへ戻す。
    ev_new_headers: Vec<(u64, Vec<h3::Header>, bool)>,
    /// F-168 P3: `process_h3_events` の Finished ストリーム ID 一時収集バッファ（再利用）。
    /// 用法は `ev_new_headers` と同じ。
    ev_finished: Vec<u64>,
    /// F-168 P3: `process_h3_events` の Reset ストリーム ID 一時収集バッファ（再利用）。
    /// 用法は `ev_new_headers` と同じ。
    ev_reset: Vec<u64>,
}

impl Http3Handler {
    /// 新しいハンドラーを作成
    fn new(
        conn: quiche::Connection,
        peer_addr: SocketAddr,
        notify: crate::http3_stream::ConnWaker,
        backend_spawner: crate::http3_stream::BackendSpawner,
        key: crate::http3_stream::ConnKey,
    ) -> Self {
        Self {
            conn,
            h3_conn: None,
            client_ip: crate::http_utils::IpStr::new(peer_addr.ip()),
            peer_addr,
            partial_responses: StreamMap::default(),
            proxy_streams: StreamMap::default(),
            buffered_reqs: StreamMap::default(),
            stream_bodies: StreamMap::default(),
            notify,
            backend_spawner,
            _conn_metric: Http3ActiveConnGuard::new(),
            metric_open_streams: HashSet::default(),
            dirty: false,
            timer_deadline: None,
            key,
            ev_new_headers: Vec::new(),
            ev_finished: Vec::new(),
            ev_reset: Vec::new(),
        }
    }

    /// リクエストストリームをメトリクス open として計上（二重 open 防止）
    #[inline]
    fn metric_stream_open(&mut self, stream_id: u64) {
        if self.metric_open_streams.insert(stream_id) {
            http3_stream_opened();
        }
    }

    /// リクエストストリームをメトリクス close として計上
    #[inline]
    fn metric_stream_close(&mut self, stream_id: u64) {
        if self.metric_open_streams.remove(&stream_id) {
            http3_stream_closed();
        }
    }

    /// HTTP/3 コネクションを初期化（QUIC 確立後）
    fn init_h3(&mut self) -> io::Result<()> {
        if self.h3_conn.is_none() && self.conn.is_established() && !self.conn.is_closed() {
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
}

impl Drop for Http3Handler {
    fn drop(&mut self) {
        // 接続破棄時に未 close のストリームゲージを一括補正（リーク防止）
        let n = self.metric_open_streams.len();
        self.metric_open_streams.clear();
        http3_streams_closed_n(n);
    }
}

impl Http3Handler {
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
    ///
    /// F-151: 戻り値は「1 件でも仕事をしたか」（`h3.poll()` が 1 件でもイベントを返した /
    /// バッファ経路リクエストを処理した / 部分レスポンスを進めた のいずれか）。
    ///
    /// **B-12 再発防止の不変条件**: h3 のイベントは `poll()` でしか取り出せず、
    /// `drive_proxy_streams` の `recv_body`（`h3.poll()` の外）がストリームを進めると
    /// イベントが内部キューに滞留したまま誰も取り出さない状態になり得る（詳細は
    /// `drive_request_pump` のコメント）。「仕事をしたら必ずもう一度 poll される」ことを
    /// メインループのダーティ集合再投入（`did_work` → dirty のまま維持）で保証し、
    /// イベントが残っているのにダーティを降ろす経路を構造的に排除する。
    async fn process_h3_events(&mut self) -> io::Result<bool> {
        // 新規 Headers（stream_id, headers, more_frames）と Finished / Reset を収集。
        // F-168 P3: リクエストごとの `Vec` 確保をなくすため `Http3Handler` の再利用バッファを
        // 借り出す（`mem::take`）。関数末尾で `clear()` して容量を保持したまま戻す。
        // ※ 早期 return がある場合、その経路では戻せず次回は空 `Vec`（容量ロス）になり得るが、
        //   正しさには影響しない。本関数唯一の早期 return（`?`、後述）は
        //   3 バッファとも使い切った後なので、実際には全パスで戻せている。
        let mut new_headers = std::mem::take(&mut self.ev_new_headers);
        let mut finished = std::mem::take(&mut self.ev_finished);
        let mut reset = std::mem::take(&mut self.ev_reset);
        let mut did_work = false;

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
                        did_work = true;
                        new_headers.push((stream_id, list, more_frames));
                    }
                    Ok((stream_id, h3::Event::Data)) => {
                        did_work = true;
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
                    Ok((stream_id, h3::Event::Finished)) => {
                        did_work = true;
                        finished.push(stream_id);
                    }
                    Ok((stream_id, h3::Event::Reset(_))) => {
                        did_work = true;
                        reset.push(stream_id);
                    }
                    Ok((_flow_id, h3::Event::GoAway)) => did_work = true,
                    Ok((_, h3::Event::PriorityUpdate)) => did_work = true,
                    Err(h3::Error::Done) => break,
                    Err(e) => {
                        warn!("[HTTP/3] h3 poll error: {}", e);
                        break;
                    }
                }
            }
        }

        // --- 新規 Headers を分類して振り分け ---
        // F-168 P3: `drain(..)` で回すことで `new_headers` 自体の確保容量を維持する
        // （ムーブで回すと Vec の所有権ごと失われ、次回また確保が必要になる）。
        for (stream_id, headers, more_frames) in new_headers.drain(..) {
            // F-99: リクエストストリーム open をメトリクス計上
            self.metric_stream_open(stream_id);
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
                    // 即時応答済み（セキュリティ拒否等）— ストリーム close
                    self.metric_stream_close(stream_id);
                }
            }
        }
        // new_headers は drain 済みで既に空。容量を保持したままフィールドへ戻す。
        self.ev_new_headers = new_headers;

        // --- Finished / Reset 反映 ---
        for stream_id in finished.drain(..) {
            if let Some(ps) = self.proxy_streams.get_mut(&stream_id) {
                ps.req_eof_seen = true;
            } else if let Some(br) = self.buffered_reqs.get_mut(&stream_id) {
                br.end = true;
            }
        }
        self.ev_finished = finished;
        for stream_id in reset.drain(..) {
            // ストリームを破棄（チャネル drop でバックエンドタスクも中断）。
            self.proxy_streams.remove(&stream_id);
            self.buffered_reqs.remove(&stream_id);
            self.stream_bodies.remove(&stream_id);
            self.metric_stream_close(stream_id);
        }
        self.ev_reset = reset;

        // --- 完了したバッファ経路リクエストを処理 ---
        let ready: Vec<u64> = self
            .buffered_reqs
            .iter()
            .filter(|(_, b)| b.end)
            .map(|(k, _)| *k)
            .collect();
        for stream_id in ready {
            did_work = true;
            let br = self.buffered_reqs.remove(&stream_id).unwrap();
            let body = self
                .stream_bodies
                .remove(&stream_id)
                .map(|b| b.to_vec())
                .unwrap_or_default();
            self.handle_request(stream_id, &br.headers, &body).await?;
            self.metric_stream_close(stream_id);
        }

        // 部分的なレスポンスを送信（非ストリーミング経路）。
        if self.flush_partial_responses()? {
            did_work = true;
        }

        // ストリーミング駆動はメインループの毎イテレーション drive で行う（通知/タイムアウト時も
        // 確実に進めるため。ここで重複呼び出ししない）。

        Ok(did_work)
    }

    /// すべてのストリーミングストリームを 1 回駆動する（req pump + resp flush）。
    ///
    /// メインループから毎イテレーション呼ばれ、フロー制御に従って `recv_body`→req チャネル、
    /// resp チャネル→`send_response`/`send_body` を進める。完了したストリームは除去する。
    ///
    /// F-151: 戻り値は「1 件でも仕事をしたか」（ストリームへ 1 バイトでも書けた / 完了した /
    /// 保留レスポンスを進めた のいずれか）。メインループはこれが `true` の間、当該接続を
    /// ダーティ集合から降ろさない（B-12 再発防止）。
    fn drive_proxy_streams(&mut self) -> bool {
        let h3 = match self.h3_conn.as_mut() {
            Some(h) => h,
            None => return false,
        };
        let conn = &mut self.conn;
        let mut done: Vec<u64> = Vec::new();
        let mut did_work = false;
        for (&stream_id, ps) in self.proxy_streams.iter_mut() {
            let (work, is_done) = drive_proxy_stream(h3, conn, stream_id, ps);
            did_work |= work;
            if is_done {
                done.push(stream_id);
            }
        }
        if !done.is_empty() {
            // ストリーム完了自体も「仕事をした」に含める。
            did_work = true;
        }
        for stream_id in done {
            debug!("[HTTP/3] streaming proxy stream {} done", stream_id);
            self.proxy_streams.remove(&stream_id);
            self.metric_stream_close(stream_id);
        }
        did_work
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

        // F-101: H1 と同等のリクエストヘッダサイズ上限（RFC 6585 / 431）。
        // 巨大 QPACK 展開ヘッダによるメモリ枯渇をストリーム処理前に拒否する。
        let header_block = h3_request_header_block_size(headers);
        if header_block > MAX_HEADER_SIZE {
            warn!(
                "[HTTP/3] request header block too large: {} bytes (limit {})",
                header_block, MAX_HEADER_SIZE
            );
            let path_too_long = path.len() > MAX_HEADER_SIZE;
            let (status, msg): (u16, &[u8]) = if path_too_long {
                (414, b"URI Too Long")
            } else {
                (431, b"Request Header Fields Too Large")
            };
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
                self.client_ip.as_str(),
                "",
            );
            return Decision::Handled;
        }

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

        // gRPC（トレーラー）はバッファ経路。Full バッファでも gRPC は専用経路で処理される。
        #[cfg(feature = "grpc")]
        let is_grpc = Self::is_grpc_request(headers);
        #[cfg(not(feature = "grpc"))]
        let is_grpc = false;

        // バッファリング full は gRPC 以外で全バッファ経路（F-97: gRPC は Full をバイパス）。
        if buffering.mode == crate::buffering::BufferingMode::Full && !is_grpc {
            return Decision::Buffer;
        }
        // WASM モジュール適用ありはボディ全体が必要 → バッファ経路。
        #[cfg(feature = "wasm")]
        if _modules
            .as_deref()
            .map(|m: &Vec<crate::wasm_plugin_config::ModuleRef>| !m.is_empty())
            .unwrap_or(false)
        {
            return Decision::Buffer;
        }
        // gRPC はトレーラー処理のためバッファ経路（ストリーミング Decision の対象外）
        if is_grpc {
            return Decision::Buffer;
        }

        // セキュリティチェック（ストリーミング適格は早期拒否でアップロードを溜めない）。
        let security = backend.security();
        let check = check_security(
            security,
            self.client_ip.as_str(),
            method,
            content_length,
            false,
        );
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
                self.client_ip.as_str(),
                "",
            );
            return Decision::Handled;
        }

        // サーバ選択（F-97: Consistent Hash header/cookie キー対応）。
        let server = match upstream_group.select_with_header_fn(self.client_ip.as_str(), |name| {
            headers
                .iter()
                .find(|h| h.name().eq_ignore_ascii_case(name))
                .map(|h| h.value())
        }) {
            Some(s) => s.clone(),
            None => return Decision::Buffer, // handle_request -> 502
        };

        // B-84: h2c 上流はストリーミング経路が扱えない（BackendTaskParams に use_h2c が無く
        // HTTP/1.1 を送ってしまうため、h2c 専用サーバに切られて 502 になる）。h2c 対応済みの
        // バッファ経路（handle_request -> proxy_to_h2c_backend_async）へ回す。
        // HTTP/2 クライアント経路（proxy.rs::h2_proxy_h2c）も同じくバッファ型なので、
        // これで HTTP/2 クライアントと HTTP/3 クライアントの挙動が揃う。
        if server.target.use_h2c || upstream_group.use_h2c() {
            return Decision::Buffer;
        }

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
    /// `handle_request_impl` を呼び出し、**成功・失敗（`?` による早期 return）を問わず**
    /// 最後に一度だけ `on_log`（`finish_h3_wasm_lifecycle`）を呼ぶ。
    ///
    /// F-132: `handle_request_impl` 内には `self.send_response(...)?` 等、`?` で早期 return
    /// する箇所が多数あり、そこに個別に `on_log` 呼び出しを仕込むと取りこぼしうる
    /// （WASM コンテキストのリークに直結する）。このラッパで囲むことで、
    /// 離脱点の数に関係なく **1 リクエストにつきちょうど 1 回** だけ呼ばれることを構造的に
    /// 保証する（`handle_request_impl` 側は `finish_h3_wasm_lifecycle` を呼ばない）。
    async fn handle_request(
        &mut self,
        stream_id: u64,
        headers: &[h3::Header],
        request_body: &[u8],
    ) -> io::Result<()> {
        #[cfg(feature = "wasm")]
        let mut wasm_modules_to_apply: Option<
            Arc<Vec<crate::wasm_plugin_config::ModuleRef>>,
        > = None;

        let result = self
            .handle_request_impl(
                stream_id,
                headers,
                request_body,
                #[cfg(feature = "wasm")]
                &mut wasm_modules_to_apply,
            )
            .await;

        #[cfg(feature = "wasm")]
        finish_h3_wasm_lifecycle(&wasm_modules_to_apply).await;

        result
    }

    async fn handle_request_impl(
        &mut self,
        stream_id: u64,
        headers: &[h3::Header],
        request_body: &[u8],
        #[cfg(feature = "wasm")] wasm_modules_to_apply: &mut Option<
            Arc<Vec<crate::wasm_plugin_config::ModuleRef>>,
        >,
    ) -> io::Result<()> {
        // HTTP/3コネクションが確立されていなければ何もしない
        if self.h3_conn.is_none() {
            return Ok(());
        }

        // F-101: バッファ経路でもヘッダサイズ上限を enforce（classify を経由しないケース）
        let header_block = h3_request_header_block_size(headers);
        if header_block > MAX_HEADER_SIZE {
            let path_len = headers
                .iter()
                .find(|h| h.name() == b":path")
                .map(|h| h.value().len())
                .unwrap_or(0);
            if path_len > MAX_HEADER_SIZE {
                self.send_error_response(stream_id, 414, b"URI Too Long")?;
            } else {
                self.send_error_response(stream_id, 431, b"Request Header Fields Too Large")?;
            }
            return Ok(());
        }

        // ヘッダーを解析（`headers` を借用するだけで、リクエストごとに `Vec` へコピーしない）
        let mut method: Option<&[u8]> = None;
        let mut path: Option<&[u8]> = None;
        let mut authority: Option<&[u8]> = None;
        let mut content_length: usize = 0;
        let mut accept_encoding: Option<&[u8]> = None;
        let mut user_agent: &[u8] = &[];

        for header in headers {
            match header.name() {
                b":method" => method = Some(header.value()),
                b":path" => path = Some(header.value()),
                b":authority" => authority = Some(header.value()),
                b"content-length" => {
                    if let Ok(s) = std::str::from_utf8(header.value()) {
                        content_length = s.parse().unwrap_or(0);
                    }
                }
                name if name.eq_ignore_ascii_case(b"accept-encoding") => {
                    accept_encoding = Some(header.value());
                }
                name if name.eq_ignore_ascii_case(b"user-agent") => {
                    user_agent = header.value();
                }
                _ => {}
            }
        }

        // クライアントの Accept-Encoding を解析
        let client_encoding = accept_encoding
            .map(AcceptedEncoding::parse)
            .unwrap_or(AcceptedEncoding::Identity);

        let method: &[u8] = method.unwrap_or(b"GET");
        let path: &[u8] = path.unwrap_or(b"/");
        let authority: &[u8] = authority.unwrap_or_default();

        // 処理開始時刻
        let start_time = Instant::now();

        debug!(
            "[HTTP/3] Request: {} {} (stream {})",
            String::from_utf8_lossy(method),
            String::from_utf8_lossy(path),
            stream_id
        );

        // F-97: :authority と Host が矛盾するリクエストを 400 で拒否
        let host_hdr = headers.iter().find_map(|h| {
            if h.name().eq_ignore_ascii_case(b"host") {
                Some(h.value())
            } else {
                None
            }
        });
        if crate::http_utils::authority_host_mismatch(authority, host_hdr) {
            self.send_error_response(stream_id, 400, b"Bad Request: :authority/Host mismatch")?;
            let user_agent_slice: &[u8] = if user_agent.is_empty() {
                &[]
            } else {
                user_agent
            };
            log_access(
                method,
                authority,
                path,
                user_agent_slice,
                content_length as u64,
                400,
                0,
                start_time,
                self.client_ip.as_str(),
                "",
            );
            return Ok(());
        }

        // gRPC リクエスト検出フラグ
        #[cfg(feature = "grpc")]
        let is_grpc = Self::is_grpc_request(headers);
        #[cfg(not(feature = "grpc"))]
        let _is_grpc = false;

        #[cfg(feature = "grpc")]
        if is_grpc {
            debug!(
                "[HTTP/3] gRPC request detected: {}",
                String::from_utf8_lossy(path)
            );
        }

        // メトリクスエンドポイント（設定可能なパス）
        {
            let config = CURRENT_CONFIG.load();
            let prom_config = &config.prometheus_config;

            let path_str = std::str::from_utf8(path).unwrap_or("/");
            if prom_config.enabled && path_str == prom_config.path && method == b"GET" {
                // IPアドレス制限チェック
                if !prom_config.is_ip_allowed(self.client_ip.as_str()) {
                    self.send_error_response(stream_id, 403, b"Forbidden")?;
                    let user_agent_slice: &[u8] = if user_agent.is_empty() {
                        &[]
                    } else {
                        user_agent
                    };
                    log_access(
                        method,
                        authority,
                        path,
                        user_agent_slice,
                        request_body.len() as u64,
                        403,
                        9,
                        start_time,
                        self.client_ip.as_str(),
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
                    user_agent
                };
                log_access(
                    method,
                    authority,
                    path,
                    user_agent_slice,
                    request_body.len() as u64,
                    200,
                    body.len() as u64,
                    start_time,
                    self.client_ip.as_str(),
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
        let path_without_query = query_start_pos.map(|i| &path[..i]).unwrap_or(path);

        let backend_result = find_backend_unified(
            authority,
            path_without_query,
            method,
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
                    String::from_utf8_lossy(authority)
                );
                find_backend_unified(
                    b"",
                    path_without_query,
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

        // F-169: File/MemoryFile バックエンドの圧縮ネゴシエーションに使う
        // （従来は破棄していたため、HTTP/3 の静的配信では圧縮が一切効いていなかった）。
        let (prefix, backend, route_compression) = match backend_result {
            Some(b) => b,
            None => {
                debug!(
                    "[HTTP/3] No backend found for authority='{}', path='{}'",
                    String::from_utf8_lossy(authority),
                    String::from_utf8_lossy(path)
                );

                // gRPC リクエストの場合は gRPC エラーレスポンスを返す
                #[cfg(feature = "grpc")]
                if is_grpc {
                    // UNIMPLEMENTED (12) - サービス/メソッドが見つからない
                    self.send_grpc_response(stream_id, &[], None, 12, Some("Service not found"))?;
                    let user_agent_slice: &[u8] = if user_agent.is_empty() {
                        &[]
                    } else {
                        user_agent
                    };
                    log_access(
                        method,
                        authority,
                        path,
                        user_agent_slice,
                        request_body.len() as u64,
                        200,
                        0,
                        start_time,
                        self.client_ip.as_str(),
                        "",
                    );
                    return Ok(());
                }

                self.send_error_response(stream_id, 404, b"Not Found")?;
                let user_agent_slice: &[u8] = if user_agent.is_empty() {
                    &[]
                } else {
                    user_agent
                };
                log_access(
                    method,
                    authority,
                    path,
                    user_agent_slice,
                    request_body.len() as u64,
                    404,
                    9,
                    start_time,
                    self.client_ip.as_str(),
                    "",
                );
                return Ok(());
            }
        };

        // セキュリティチェック
        let security = backend.security();
        let check_result = check_security(
            security,
            self.client_ip.as_str(),
            method,
            content_length,
            false,
        );

        if check_result != SecurityCheckResult::Allowed {
            let status = check_result.status_code();
            let msg = check_result.message();
            self.send_error_response(stream_id, status, msg)?;
            let user_agent_slice: &[u8] = if user_agent.is_empty() {
                &[]
            } else {
                user_agent
            };
            log_access(
                method,
                authority,
                path,
                user_agent_slice,
                request_body.len() as u64,
                status,
                msg.len() as u64,
                start_time,
                self.client_ip.as_str(),
                "",
            );
            return Ok(());
        }

        // WASM モジュール適用（B-38: リクエストヘッダ変更 + レスポンスヘッダ変更）
        // F-132: `wasm_modules_to_apply` は呼び出し元（`handle_request`）が保持する out
        // パラメータ。ここで `Some` にセットしておけば、この後どの `?` で早期 return しても
        // 呼び出し元側で必ず一度だけ `on_log` が呼ばれる。
        #[cfg(feature = "wasm")]
        let mut wasm_request_headers: Option<Vec<(Vec<u8>, Vec<u8>)>> = None;
        // F-132: WASM on_request_body で書き換えられた本文（適用時のみ Some）。
        #[cfg(feature = "wasm")]
        let mut wasm_request_body_override: Option<Bytes> = None;
        #[cfg(feature = "wasm")]
        {
            let config = CURRENT_CONFIG.load();
            if let Some(ref wasm_engine) = config.wasm_filter_engine {
                let path_str = std::str::from_utf8(path).unwrap_or("/");
                let method_str = std::str::from_utf8(method).unwrap_or("GET");

                // F-43: モジュールリストは Arc 共有（リクエストごとの deep copy 排除）
                let modules_to_apply = if let Some(backend_modules) = backend.modules_arc() {
                    backend_modules.clone()
                } else {
                    crate::wasm::empty_wasm_modules()
                };

                if !modules_to_apply.is_empty() {
                    *wasm_modules_to_apply = Some(modules_to_apply.clone());

                    let headers_vec: Vec<(Vec<u8>, Vec<u8>)> = headers
                        .iter()
                        .filter(|h| !h.name().starts_with(b":"))
                        .map(|h| (h.name().to_vec(), h.value().to_vec()))
                        .collect();

                    let wasm_result = wasm_engine
                        .on_request_headers_with_modules(
                            &modules_to_apply,
                            &std::sync::Arc::from(path_str),
                            &std::sync::Arc::from(method_str),
                            headers_vec,
                            &std::sync::Arc::from(self.client_ip.as_str()),
                            request_body.is_empty(),
                        )
                        .await;

                    match wasm_result {
                        crate::wasm::FilterResult::LocalResponse(resp) => {
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
                                user_agent
                            };
                            log_access(
                                method,
                                authority,
                                path,
                                user_agent_slice,
                                request_body.len() as u64,
                                resp.status_code,
                                resp.body.len() as u64,
                                start_time,
                                self.client_ip.as_str(),
                                "",
                            );
                            // F-132: on_log は呼び出し元の `handle_request` ラッパが
                            // `?`/早期 return を問わず最後に一度だけ呼ぶ
                            // （`*wasm_modules_to_apply` は既にセット済み）。
                            return Ok(());
                        }
                        crate::wasm::FilterResult::Pause => {
                            warn!("WASM module requested pause, but async operations are not yet supported");
                        }
                        crate::wasm::FilterResult::Continue {
                            headers: modified, ..
                        } => {
                            // B-38: 変更後ヘッダを上流リクエストへ反映
                            wasm_request_headers = Some(modified);

                            // F-132: リクエストボディフィルタ（HTTP/3 は WASM 適用時に
                            // 必ず Decision::Buffer に落ちるためボディ全体がメモリ上にある。
                            // end_of_stream=true の 1 回呼びで実装できる）。
                            if !request_body.is_empty() {
                                match crate::wasm::http_executor::apply_wasm_request_body(
                                    wasm_engine,
                                    &modules_to_apply,
                                    Bytes::copy_from_slice(request_body),
                                    true,
                                )
                                .await
                                {
                                    crate::wasm::http_executor::WasmBodyOutcome::Continue(b) => {
                                        wasm_request_body_override = Some(b);
                                    }
                                    crate::wasm::http_executor::WasmBodyOutcome::LocalResponse(
                                        resp,
                                    ) => {
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
                                            user_agent
                                        };
                                        log_access(
                                            method,
                                            authority,
                                            path,
                                            user_agent_slice,
                                            request_body.len() as u64,
                                            resp.status_code,
                                            resp.body.len() as u64,
                                            start_time,
                                            self.client_ip.as_str(),
                                            "",
                                        );
                                        // F-132: on_log は呼び出し元の `handle_request`
                                        // ラッパが最後に一度だけ呼ぶ。
                                        return Ok(());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // バックエンド処理
        let (status, resp_size) = match backend {
            Backend::Proxy(upstream_group, security, path_compression, _buffering, _cache, _) => {
                // B-74: H2C 接続プールの max_idle/idle_timeout 用。http2 feature 無効時は
                // H2C 中継自体が無いため未使用（unused_variables 警告回避）。
                #[cfg(not(feature = "http2"))]
                let _ = &security;
                debug!("[HTTP/3] Starting proxy request to upstream group");

                // HTTP/3専用圧縮設定を解決
                // 優先順位: パス設定 > HTTP/3設定 > デフォルト
                let config = CURRENT_CONFIG.load();
                let effective_compression =
                    resolve_http3_compression_config(&path_compression, &config.http3_config);

                // F-132: WASM on_request_body で書き換えられた本文があればそれを使う。
                #[cfg(feature = "wasm")]
                let effective_request_body: &[u8] = wasm_request_body_override
                    .as_deref()
                    .unwrap_or(request_body);
                #[cfg(not(feature = "wasm"))]
                let effective_request_body: &[u8] = request_body;

                let result = self
                    .handle_proxy(
                        stream_id,
                        &upstream_group,
                        #[cfg(feature = "http2")]
                        &security,
                        &effective_compression,
                        client_encoding,
                        method,
                        path,
                        &prefix,
                        headers,
                        effective_request_body,
                        #[cfg(feature = "wasm")]
                        wasm_modules_to_apply.as_ref(),
                        #[cfg(feature = "wasm")]
                        wasm_request_headers.as_deref(),
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
                let path_str = std::str::from_utf8(path).unwrap_or("/");
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
                    // F-132: h1/h2 と同様、静的配信（Backend::File 系）にも WASM
                    // on_response_headers を適用する。
                    let mut header_store: Vec<(Vec<u8>, Vec<u8>)> = vec![
                        (b"content-type".to_vec(), mime_type.as_bytes().to_vec()),
                        (b"server".to_vec(), b"veil/http3".to_vec()),
                    ];
                    for (k, v) in &security.add_response_headers {
                        header_store.push((k.as_bytes().to_vec(), v.as_bytes().to_vec()));
                    }

                    // F-169: HTTP/3 の File 系バックエンドは従来ここで圧縮設定
                    // （`route_compression`）を破棄しており、h2 と違って圧縮が
                    // 一切効いていなかった。h2 (`build_h2_compressed_file_response`)
                    // と同じネゴシエーションを適用する。MemoryFile はファイルシステム
                    // パスを持たないため圧縮結果キャッシュ（`cache::compressed`）は
                    // 使えない（毎回圧縮する。h2 側の MemoryFile 経路と同じ扱い）。
                    let file_compression =
                        resolve_http3_compression_config(&route_compression, &config.http3_config);
                    let should_compress = file_compression.should_compress(
                        client_encoding,
                        Some(mime_type.as_bytes()),
                        Some(data.len()),
                        None,
                    );

                    let response_body: Bytes = if let Some(enc) = should_compress {
                        let encoding_name: &[u8] = match enc {
                            AcceptedEncoding::Zstd => b"zstd",
                            AcceptedEncoding::Brotli => b"br",
                            AcceptedEncoding::Gzip => b"gzip",
                            AcceptedEncoding::Deflate => b"deflate",
                            AcceptedEncoding::Identity => b"",
                        };
                        if !encoding_name.is_empty() {
                            header_store
                                .push((b"content-encoding".to_vec(), encoding_name.to_vec()));
                            header_store.push((b"vary".to_vec(), b"Accept-Encoding".to_vec()));
                        }
                        Bytes::from(compress_body_h3(data.as_slice(), enc, &file_compression))
                    } else {
                        // Arc<Vec<u8>> の参照カウントクローンのみ（ディープコピー無し）。
                        Bytes::from_owner(ArcVecBytes(data.clone()))
                    };

                    #[cfg(feature = "wasm")]
                    if let Some(modules) = wasm_modules_to_apply.as_ref() {
                        header_store =
                            apply_h3_wasm_response_headers(modules, 200, header_store).await;
                    }

                    let resp_headers: Vec<(&[u8], &[u8])> = header_store
                        .iter()
                        .map(|(k, v)| (k.as_slice(), v.as_slice()))
                        .collect();

                    self.send_response(stream_id, 200, &resp_headers, Some(&response_body))?;
                    (200, response_body.len())
                }
            }
            Backend::SendFile(
                base_path,
                is_dir,
                index_file,
                security,
                _cache,
                open_file_cache_config,
                canonical_base,
                static_file_cache_config,
                _,
            ) => {
                // F-169: HTTP/3 専用圧縮設定を解決（Proxy 経路の `handle_proxy` と同じ
                // 優先順位: パス設定 > HTTP/3 設定 > デフォルト）。従来 SendFile は
                // `route_compression` を破棄しており圧縮が一切効いていなかった。
                let file_compression =
                    resolve_http3_compression_config(&route_compression, &config.http3_config);
                self.handle_sendfile(SendFileRequest {
                    stream_id,
                    base_path: &base_path,
                    is_dir,
                    index_file: index_file.as_deref(),
                    req_path: path,
                    prefix: &prefix,
                    security: &security,
                    compression: &file_compression,
                    client_encoding,
                    open_file_cache_config: open_file_cache_config.as_deref(),
                    canonical_base: canonical_base.as_deref(),
                    static_file_cache_config: static_file_cache_config.as_deref(),
                    #[cfg(feature = "wasm")]
                    wasm_modules: wasm_modules_to_apply.as_ref(),
                })
                .await
                .unwrap_or((404, 9))
            }
            Backend::Redirect(redirect_url, status_code, preserve_path, _) => self
                .handle_redirect(
                    stream_id,
                    &redirect_url,
                    status_code,
                    preserve_path,
                    path,
                    &prefix,
                )
                .unwrap_or((500, 0)),
        };

        let user_agent_slice: &[u8] = if user_agent.is_empty() {
            &[]
        } else {
            user_agent
        };
        log_access(
            method,
            authority,
            path,
            user_agent_slice,
            request_body.len() as u64,
            status,
            resp_size as u64,
            start_time,
            self.client_ip.as_str(),
            "",
        );
        // F-132: on_log は呼び出し元の `handle_request` ラッパが最後に一度だけ呼ぶ。
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
        // 借用ヘッダ（`HeaderRef`）で渡す。`h3::Header::new` は名前と値を毎回 `Vec` に
        // コピーするため、応答ごとにヘッダ数 × 2 回の確保になっていた。
        let mut h3_headers: Vec<h3::HeaderRef<'_>> = Vec::with_capacity(headers.len() + 2);
        h3_headers.push(h3::HeaderRef::new(b":status", status_str.as_bytes()));

        // B-46: headers に既に content-length が含まれるか、既存のヘッダー走査
        // ループに検査を織り込んで判定する（追加の走査を増やさない）。
        let mut has_content_length = false;
        for (name, value) in headers {
            if *name != b":status" {
                if name.eq_ignore_ascii_case(b"content-length") {
                    has_content_length = true;
                }
                h3_headers.push(h3::HeaderRef::new(name, value));
            }
        }

        // Content-Length を追加（itoa::Buffer使用）。
        // B-46: プロキシ応答では merge_response_headers_and_trailers が
        // バックエンド（例: nginx）由来の content-length を残すため、ここで
        // 無条件に追加すると content-length が重複する。nghttp3 は重複
        // content-length を malformed message として H3_MESSAGE_ERROR で
        // 拒否し、ヘッダ受信直後にストリームを停止する（ボディ 0 バイト・
        // 全リクエスト失敗）。headers に既に含まれる場合は追加しない。
        let mut len_buf = itoa::Buffer::new();
        if !has_content_length {
            if let Some(body_data) = body {
                let len_str = len_buf.format(body_data.len());
                h3_headers.push(h3::HeaderRef::new(b"content-length", len_str.as_bytes()));
            }
        }

        // ヘッダー送信
        let has_body = body.is_some() && body.is_some_and(|b| !b.is_empty());
        match h3_conn.send_response(&mut self.conn, stream_id, &h3_headers, !has_body) {
            Ok(()) => {
                debug!("[HTTP/3] Response headers sent for stream {}", stream_id);
            }
            Err(h3::Error::StreamBlocked) => {
                // B-43: ヘッダ未送出のまま保留する。従来はボディだけ保存し
                // 再送で send_body を先に呼んで FrameUnexpected になっていた。
                // 構築済みヘッダを move で保存し、head=Some の間はヘッダ未送出とみなす。
                // ボディ無し応答（リダイレクト・エラー）も保存して無言消失を防ぐ。
                debug!("[HTTP/3] Stream {} blocked, will retry later", stream_id);
                self.partial_responses.insert(
                    stream_id,
                    PartialResponse {
                        // 保留時だけ所有ヘッダへ変換する（稀な経路）。
                        head: Some(
                            h3_headers
                                .iter()
                                .map(|h| h3::Header::new(h.name(), h.value()))
                                .collect(),
                        ),
                        body: body.map(|b| b.to_vec()).unwrap_or_default(),
                        written: 0,
                    },
                );
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
                        // 部分的にしか送信できなかった場合（B-43: ヘッダは送出済みなので head=None）
                        if written < body_data.len() {
                            self.partial_responses.insert(
                                stream_id,
                                PartialResponse {
                                    head: None,
                                    body: body_data.to_vec(),
                                    written,
                                },
                            );
                        }
                    }
                    Err(h3::Error::Done) => {
                        // バッファがいっぱい、後で再送（B-43: ヘッダは送出済みなので head=None）
                        debug!(
                            "[HTTP/3] Body buffer full for stream {}, queuing for later",
                            stream_id
                        );
                        self.partial_responses.insert(
                            stream_id,
                            PartialResponse {
                                head: None,
                                body: body_data.to_vec(),
                                written: 0,
                            },
                        );
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
    /// HTTP/3 では初期 HEADERS の後、ボディ（任意）を送り、
    /// **`send_additional_headers` で trailers を fin=true で送出**する（B-41）。
    /// quiche の `send_response` は初期応答専用で、2 回目に使うと失敗しストリームが
    /// 閉じられずクライアントがハングする。
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

        // 1. 初期ヘッダ（:status + content-type）。grpc-status/message は trailers 専用。
        let mut h3_headers = vec![
            h3::Header::new(b":status", b"200"),
            h3::Header::new(b"content-type", b"application/grpc"),
        ];

        for (name, value) in filter_h3_grpc_initial_headers(headers) {
            h3_headers.push(h3::Header::new(name, value));
        }

        let has_body = body.is_some_and(|b| !b.is_empty());

        // ボディ有無に関わらず初期ヘッダは fin=false（trailers で終端）
        if let Err(e) = h3_conn.send_response(&mut self.conn, stream_id, &h3_headers, false) {
            warn!("[HTTP/3] gRPC send_response error: {}", e);
            return Ok(());
        }

        // 2. ボディ（fin=false — trailers が続く）
        if has_body {
            if let Some(body_data) = body {
                if let Err(e) = h3_conn.send_body(&mut self.conn, stream_id, body_data, false) {
                    warn!("[HTTP/3] gRPC send_body error: {}", e);
                }
            }
        }

        // 3. trailers（fin=true）
        self.send_grpc_trailers_internal(stream_id, grpc_status, grpc_message)
    }

    /// gRPC トレイラーを `send_additional_headers` で送信しストリームを終了（B-41）。
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

        let trailers: Vec<h3::Header> = trailer_pairs
            .iter()
            .map(|(n, v)| h3::Header::new(n.as_slice(), v.as_slice()))
            .collect();

        // B-41: trailers は send_additional_headers（is_trailer_section=true, fin=true）
        // send_response の再利用は不可（初期応答専用 API）
        if let Err(e) =
            h3_conn.send_additional_headers(&mut self.conn, stream_id, &trailers, true, true)
        {
            warn!(
                "[HTTP/3] gRPC trailers send_additional_headers error: {}",
                e
            );
            // フォールバック: 空ボディ + fin でストリームを閉じ、クライアントハングを防ぐ
            if let Err(e2) = h3_conn.send_body(&mut self.conn, stream_id, &[], true) {
                debug!("[HTTP/3] gRPC trailers fin fallback error: {:?}", e2);
            }
        }

        Ok(())
    }

    /// プロキシ処理（HTTP/1.1 または H2C バックエンドへの変換）
    ///
    /// - 通常: HTTP/1.1 で上流へ転送
    /// - `use_h2c`: H2C (Prior Knowledge) で上流へ転送（B-39: gRPC over HTTP/3）
    /// - WASM: リクエスト/レスポンスヘッダ変更を適用（B-38）
    async fn handle_proxy(
        &mut self,
        stream_id: u64,
        upstream_group: &Arc<UpstreamGroup>,
        // B-74: H2C 接続プール（H2C_POOL）の max_idle/idle_timeout を引くためだけに使う。
        // http2 feature 無効時は H2C 中継自体が存在しないため未使用になる。
        #[cfg(feature = "http2")] security: &SecurityConfig,
        compression: &CompressionConfig,
        client_encoding: AcceptedEncoding,
        method: &[u8],
        req_path: &[u8],
        prefix: &[u8],
        headers: &[h3::Header],
        request_body: &[u8],
        #[cfg(feature = "wasm")] wasm_modules: Option<
            &std::sync::Arc<Vec<crate::wasm_plugin_config::ModuleRef>>,
        >,
        #[cfg(feature = "wasm")] wasm_request_headers: Option<&[(Vec<u8>, Vec<u8>)]>,
    ) -> io::Result<(u16, usize)> {
        // サーバー選択（F-97: Consistent Hash header/cookie キー対応）
        let server = match upstream_group.select_with_header_fn(self.client_ip.as_str(), |name| {
            headers
                .iter()
                .find(|h| h.name().eq_ignore_ascii_case(name))
                .map(|h| h.value())
        }) {
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
        let timeout_secs = 30;

        // 上流へ送るヘッダソース（WASM 変更後 or 生 H3 ヘッダ）
        #[cfg(feature = "wasm")]
        let header_pairs: Vec<(Vec<u8>, Vec<u8>)> = if let Some(ov) = wasm_request_headers {
            ov.iter()
                .filter(|(n, _)| {
                    !n.starts_with(b":")
                        && !n.eq_ignore_ascii_case(b"connection")
                        && !n.eq_ignore_ascii_case(b"keep-alive")
                        && !n.eq_ignore_ascii_case(b"transfer-encoding")
                })
                .cloned()
                .collect()
        } else {
            headers
                .iter()
                .filter(|h| {
                    !h.name().starts_with(b":")
                        && !h.name().eq_ignore_ascii_case(b"connection")
                        && !h.name().eq_ignore_ascii_case(b"keep-alive")
                        && !h.name().eq_ignore_ascii_case(b"transfer-encoding")
                })
                .map(|h| (h.name().to_vec(), h.value().to_vec()))
                .collect()
        };
        #[cfg(not(feature = "wasm"))]
        let header_pairs: Vec<(Vec<u8>, Vec<u8>)> = headers
            .iter()
            .filter(|h| {
                !h.name().starts_with(b":")
                    && !h.name().eq_ignore_ascii_case(b"connection")
                    && !h.name().eq_ignore_ascii_case(b"keep-alive")
                    && !h.name().eq_ignore_ascii_case(b"transfer-encoding")
            })
            .map(|h| (h.name().to_vec(), h.value().to_vec()))
            .collect();

        // gRPC はサービス/メソッドのフルパスを保持（B-39）
        #[cfg(feature = "grpc")]
        let is_grpc_req = header_pairs_indicate_grpc(&header_pairs);
        #[cfg(not(feature = "grpc"))]
        let is_grpc_req = false;

        let final_path_owned =
            compute_upstream_request_path(path_str, prefix, &target.path_prefix, is_grpc_req);
        let final_path = final_path_owned.as_str();

        // B-39: H2C 上流（gRPC 等）
        let use_h2c = target.use_h2c || upstream_group.use_h2c();
        let proxy_result = if use_h2c {
            #[cfg(feature = "http2")]
            {
                proxy_to_h2c_backend_async(
                    target,
                    method,
                    final_path.as_bytes(),
                    &header_pairs,
                    request_body,
                    timeout_secs,
                    security,
                )
                .await
            }
            #[cfg(not(feature = "http2"))]
            {
                warn!("[HTTP/3] use_h2c requested but http2 feature disabled");
                Err(io::Error::other("H2C requires http2 feature"))
            }
        } else {
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

            for (name, value) in &header_pairs {
                request.extend_from_slice(name);
                request.extend_from_slice(b": ");
                request.extend_from_slice(value);
                request.extend_from_slice(b"\r\n");
            }

            if !request_body.is_empty() {
                request.extend_from_slice(b"Content-Length: ");
                let mut len_buf = itoa::Buffer::new();
                request.extend_from_slice(len_buf.format(request_body.len()).as_bytes());
                request.extend_from_slice(b"\r\n");
            }

            request.extend_from_slice(b"Connection: close\r\n\r\n");
            request.extend_from_slice(request_body);

            let tls_insecure = upstream_group.tls_insecure();
            proxy_to_backend_async_with_tls(target, request, timeout_secs, tls_insecure).await
        };

        server.release();

        match proxy_result {
            Ok(backend_result) => {
                let status_code = backend_result.status_code;
                #[cfg(feature = "wasm")]
                let mut body = backend_result.body;
                #[cfg(not(feature = "wasm"))]
                let body = backend_result.body;
                let trailers = backend_result.trailers;

                // B-38: WASM レスポンスヘッダフィルタ
                #[cfg(feature = "wasm")]
                let mut resp_header_store = backend_result.headers;
                #[cfg(feature = "wasm")]
                if let Some(modules) = wasm_modules {
                    if !modules.is_empty() {
                        resp_header_store =
                            apply_h3_wasm_response_headers(modules, status_code, resp_header_store)
                                .await;
                    }
                }
                #[cfg(not(feature = "wasm"))]
                let resp_header_store = backend_result.headers;

                // gRPC は圧縮ネゴシエーション/ボディフィルタ対象外（application/grpc、trailers は F-133）
                #[cfg(feature = "grpc")]
                let is_grpc_ct = resp_header_store
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(b"content-type"))
                    .map(|(_, v)| crate::grpc::headers::is_grpc_content_type(v))
                    .unwrap_or(false);
                #[cfg(not(feature = "grpc"))]
                let is_grpc_ct = false;

                // F-132: WASM レスポンスボディフィルタ（HTTP/3 は WASM 適用時に必ず
                // Decision::Buffer に落ちるためボディ全体がメモリ上にある。end_of_stream=true
                // の 1 回呼びで実装できる）。書き換えたら content-length を更新する（B-46）。
                #[cfg(feature = "wasm")]
                if !is_grpc_ct {
                    if let Some(modules) = wasm_modules {
                        if !modules.is_empty() {
                            let config = CURRENT_CONFIG.load();
                            if let Some(ref wasm_engine) = config.wasm_filter_engine {
                                match crate::wasm::http_executor::apply_wasm_response_body(
                                    wasm_engine,
                                    modules,
                                    Bytes::from(body),
                                    true,
                                )
                                .await
                                {
                                    crate::wasm::http_executor::WasmBodyOutcome::Continue(b) => {
                                        crate::wasm::http_executor::set_content_length_header(
                                            &mut resp_header_store,
                                            b.len(),
                                        );
                                        body = b.to_vec();
                                    }
                                    crate::wasm::http_executor::WasmBodyOutcome::LocalResponse(
                                        resp,
                                    ) => {
                                        let resp_headers: Vec<(&[u8], &[u8])> = resp
                                            .headers
                                            .iter()
                                            .map(|(k, v)| (k.as_slice(), v.as_slice()))
                                            .collect();
                                        self.send_response(
                                            stream_id,
                                            resp.status_code,
                                            &resp_headers,
                                            Some(&resp.body),
                                        )?;
                                        return Ok((resp.status_code, resp.body.len()));
                                    }
                                }
                            }
                        }
                    }
                }

                // 圧縮判定
                let mut content_type: Option<&[u8]> = None;
                let mut existing_encoding: Option<&[u8]> = None;
                for (name, value) in &resp_header_store {
                    if name.eq_ignore_ascii_case(b"content-type") {
                        content_type = Some(value.as_slice());
                    } else if name.eq_ignore_ascii_case(b"content-encoding") {
                        existing_encoding = Some(value.as_slice());
                    }
                }

                let should_compress = if is_grpc_ct {
                    None
                } else {
                    compression.should_compress(
                        client_encoding,
                        content_type,
                        Some(body.len()),
                        existing_encoding,
                    )
                };

                // レスポンスヘッダ + H2C trailers をマージ（B-39）
                let owned_headers = merge_response_headers_and_trailers(
                    &resp_header_store,
                    &trailers,
                    should_compress.is_some(),
                );

                let response_body = if let Some(enc) = should_compress {
                    compress_body_h3(&body, enc, compression)
                } else {
                    body
                };

                let resp_headers: Vec<(&[u8], &[u8])> = owned_headers
                    .iter()
                    .map(|(n, v)| (n.as_slice(), v.as_slice()))
                    .collect();

                // gRPC: trailers 用 API で終端（status は既に headers にマージ済み）
                #[cfg(feature = "grpc")]
                if is_grpc_ct && !trailers.is_empty() {
                    let grpc_status = trailers
                        .iter()
                        .find(|(n, _)| n.eq_ignore_ascii_case(b"grpc-status"))
                        .and_then(|(_, v)| std::str::from_utf8(v).ok())
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(0);
                    let grpc_message = trailers
                        .iter()
                        .find(|(n, _)| n.eq_ignore_ascii_case(b"grpc-message"))
                        .and_then(|(_, v)| String::from_utf8(v.clone()).ok());
                    // ヘッダにマージ済み + ボディ送信。status 200 で trailers も送る。
                    self.send_grpc_response(
                        stream_id,
                        &resp_headers,
                        Some(&response_body),
                        grpc_status,
                        grpc_message.as_deref(),
                    )?;
                    return Ok((status_code, response_body.len()));
                }

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
    async fn handle_sendfile(&mut self, req: SendFileRequest<'_>) -> io::Result<(u16, usize)> {
        let SendFileRequest {
            stream_id,
            base_path,
            is_dir,
            index_file,
            req_path,
            prefix,
            security,
            compression,
            client_encoding,
            open_file_cache_config,
            canonical_base,
            static_file_cache_config,
            #[cfg(feature = "wasm")]
            wasm_modules,
        } = req;
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

        // ファイルパス構築（素の join のみ。ディレクトリかどうかの判定は下の
        // `get_file_info_with_config` の非同期呼び出しへ委譲する）。
        let full_path = if is_dir {
            let mut p = base_path.to_path_buf();
            if !clean_sub.is_empty() {
                p.push(clean_sub);
            }
            p
        } else {
            if !clean_sub.is_empty() {
                self.send_error_response(stream_id, 404, b"Not Found")?;
                return Ok((404, 9));
            }
            base_path.to_path_buf()
        };

        // B-65（続き）: 従来は「メタデータ解決（offload 1）→ …index 解決…→ 本体読み込み
        // （offload 2）」で 1 リクエストあたり offload のクロススレッド往復が 2 回
        // 発生していた（`docs/backlog/bugs/B-65-freebsd-h2c-request-cost.md`）。
        // 当初の改修は `is_dir == false`（固定ファイルルート）でしか高速経路が使われず、
        // 実運用で一般的なディレクトリルートでは改善していなかった。
        //
        // `is_dir` の true/false に関わらずまず `get_static_file_with_content` を 1 回
        // 呼ぶ（両キャッシュヒット時は offload ゼロ）。ディレクトリルートの場合のみ
        // `containment` に「このルート自身の」`base_path` を渡し、高速経路（Linux）は
        // その `base_path` の dirfd に対してのみ open する（F-154:
        // `resolve::open_beneath_in_root`）ため、これ自体が per-route の封じ込めになり、
        // `readlink` 等の事後検査は不要（`cache::static_file` モジュール doc 参照）。
        // h1/h2 と同一方針。
        let content_cfg = cache::effective_static_content_cache_config(static_file_cache_config);
        let containment = is_dir.then_some(cache::RouteContainment {
            root: base_path,
            canonical_base,
        });
        let first_result = cache::get_static_file_with_content(
            &full_path,
            open_file_cache_config,
            &content_cfg,
            containment,
        )
        .await;

        // F-169: `served_path`（実際に配信するパス。ディレクトリルートで index に
        // フォールバックした場合は index 側）を圧縮結果キャッシュのキーに使う。
        // `full_path` の所有権をそのまま流用するため追加のクローンは発生しない
        // （h2 側 `h2_sendfile` と同じ方針）。
        let (data, mime_owned, served_path): (
            bytes::Bytes,
            std::sync::Arc<str>,
            std::path::PathBuf,
        ) = match first_result {
            Some(cache::StaticFileOutcome::File(info, data)) => (data, info.mime_type, full_path),
            Some(cache::StaticFileOutcome::Forbidden) => {
                self.send_error_response(stream_id, 403, b"Forbidden")?;
                return Ok((403, 9));
            }
            Some(cache::StaticFileOutcome::Directory(file_info)) => {
                // ディレクトリの場合はインデックスファイルを解決してからもう一度呼ぶ
                // （h1/h2 と同一ロジック。封じ込め検査は既に上で通過済みのため index
                // パスにも同じ containment を渡す）。
                let filename = index_file.unwrap_or("index.html");
                let index_path = file_info.canonical_path.join(filename);
                match cache::get_static_file_with_content(
                    &index_path,
                    open_file_cache_config,
                    &content_cfg,
                    containment,
                )
                .await
                {
                    Some(cache::StaticFileOutcome::File(info, data)) => {
                        (data, info.mime_type, index_path)
                    }
                    Some(cache::StaticFileOutcome::Forbidden) | None => {
                        self.send_error_response(stream_id, 403, b"Forbidden")?;
                        return Ok((403, 9));
                    }
                    Some(cache::StaticFileOutcome::Directory(_)) => {
                        // index ファイル自体がさらにディレクトリ（通常起こり得ないが、
                        // 安全側に倒して 403 とする）。
                        self.send_error_response(stream_id, 403, b"Forbidden")?;
                        return Ok((403, 9));
                    }
                }
            }
            None => {
                // ファイルが開けない場合はキャッシュを無効化（HTTP/1.1・HTTP/2 と同様）。
                cache::invalidate_file_cache(&full_path);
                cache::invalidate_content_cache(&full_path);
                self.send_error_response(stream_id, 404, b"Not Found")?;
                return Ok((404, 9));
            }
        };
        let mime_str: &str = &mime_owned;

        // F-169: 圧縮ネゴシエーション + 静的配信の圧縮結果キャッシュ
        // （`cache::compressed`、`content_cfg.enabled` = `static_file_cache` 有効時のみ）。
        // 従来この関数は圧縮設定自体を受け取っておらず、HTTP/3 の静的配信では
        // 圧縮が一切効いていなかった。
        let should_compress = compression.should_compress(
            client_encoding,
            Some(mime_str.as_bytes()),
            Some(data.len()),
            None,
        );
        let mut encoding_name: &[u8] = b"";
        let response_body: Bytes = if let Some(enc) = should_compress {
            encoding_name = match enc {
                AcceptedEncoding::Zstd => b"zstd",
                AcceptedEncoding::Brotli => b"br",
                AcceptedEncoding::Gzip => b"gzip",
                AcceptedEncoding::Deflate => b"deflate",
                AcceptedEncoding::Identity => b"",
            };
            if content_cfg.enabled {
                let level = cache::compressed::compression_level(enc, compression);
                cache::compressed::get_or_compress(&served_path, enc, level, &content_cfg, || {
                    compress_body_h3(&data, enc, compression)
                })
            } else {
                Bytes::from(compress_body_h3(&data, enc, compression))
            }
        } else {
            // Bytes::clone() は参照カウント増加のみ（ディープコピー無し）。
            data.clone()
        };

        // 応答ヘッダは借用のまま組み立てる（以前は `Vec<(Vec<u8>, Vec<u8>)>` で
        // リクエストごとに 7〜8 回確保していた）。所有バッファが要るのは F-132 の
        // WASM on_response_headers を適用するときだけ。
        let mut resp_headers: Vec<(&[u8], &[u8])> =
            Vec::with_capacity(4 + security.add_response_headers.len());
        resp_headers.push((b"content-type", mime_str.as_bytes()));
        resp_headers.push((b"server", b"veil/http3"));
        for (k, v) in &security.add_response_headers {
            resp_headers.push((k.as_bytes(), v.as_bytes()));
        }
        if !encoding_name.is_empty() {
            resp_headers.push((b"content-encoding", encoding_name));
            resp_headers.push((b"vary", b"Accept-Encoding"));
        }

        // F-132: h1/h2 と同様、静的配信にも WASM on_response_headers を適用する。
        #[cfg(feature = "wasm")]
        if let Some(modules) = wasm_modules {
            let header_store: Vec<(Vec<u8>, Vec<u8>)> = resp_headers
                .iter()
                .map(|(k, v)| (k.to_vec(), v.to_vec()))
                .collect();
            let header_store = apply_h3_wasm_response_headers(modules, 200, header_store).await;
            let owned: Vec<(&[u8], &[u8])> = header_store
                .iter()
                .map(|(k, v)| (k.as_slice(), v.as_slice()))
                .collect();
            self.send_response(stream_id, 200, &owned, Some(response_body.as_ref()))?;
            return Ok((200, response_body.len()));
        }

        self.send_response(stream_id, 200, &resp_headers, Some(response_body.as_ref()))?;
        Ok((200, response_body.len()))
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

    /// B-43: 保留中の部分レスポンスを 1 エントリ分だけ再送する共通ヘルパー。
    ///
    /// `flush_partial_responses` と `handle_writable_streams` の双方から使い、
    /// ヘッダ→ボディの送出順序を一元管理する。戻り値 `true` = 完了（エントリ削除可）。
    ///
    /// - `pr.head` が Some の間はまだ HEADERS 未送出。まず `send_response` で送る:
    ///   - Ok → head を None にしてボディ送出フェーズへ続行（ボディ空なら完了）。
    ///   - StreamBlocked → エントリを保持（`false`）し次回再試行。
    ///   - その他エラー → warn の上で破棄（`true`）。
    /// - ボディは既存どおり `send_body(fin=true)`。Ok で written 加算、全量送出で完了。
    ///   Done は保持、その他エラーは warn の上で破棄。
    fn try_flush_partial(
        h3_conn: &mut h3::Connection,
        conn: &mut quiche::Connection,
        stream_id: u64,
        pr: &mut PartialResponse,
    ) -> bool {
        // まず未送出ヘッダを送る。
        if let Some(head) = &pr.head {
            match h3_conn.send_response(conn, stream_id, head, pr.body.is_empty()) {
                Ok(()) => {
                    debug!(
                        "[HTTP/3] Deferred response headers sent for stream {}",
                        stream_id
                    );
                    pr.head = None;
                    // ボディ無し応答（リダイレクト等）はヘッダ送出で完了。
                    if pr.body.is_empty() {
                        return true;
                    }
                    // ボディありなら同一呼び出しで送出フェーズへ続行。
                }
                Err(h3::Error::StreamBlocked) => {
                    // まだ HEADERS を送れない。エントリを保持して次回再試行。
                    debug!("[HTTP/3] Stream {} still blocked (headers)", stream_id);
                    return false;
                }
                Err(e) => {
                    warn!(
                        "[HTTP/3] deferred send_response error on stream {}: {}",
                        stream_id, e
                    );
                    return true;
                }
            }
        }

        // ボディ送出。
        if pr.written < pr.body.len() {
            match h3_conn.send_body(conn, stream_id, &pr.body[pr.written..], true) {
                Ok(sent) => {
                    pr.written += sent;
                    debug!(
                        "[HTTP/3] Deferred body sent for stream {}: {}/{}",
                        stream_id,
                        pr.written,
                        pr.body.len()
                    );
                    pr.written >= pr.body.len()
                }
                Err(h3::Error::Done) => {
                    // まだブロックされている。保持して次回再試行。
                    debug!("[HTTP/3] Stream {} still blocked (body)", stream_id);
                    false
                }
                Err(e) => {
                    warn!(
                        "[HTTP/3] deferred send_body error on stream {}: {}",
                        stream_id, e
                    );
                    true
                }
            }
        } else {
            // ボディ全量送出済み。
            true
        }
    }

    /// 部分的なレスポンスをフラッシュする。
    ///
    /// F-151: 戻り値は「1 件でも進捗があったか」（ヘッダ/ボディ送出 or 完了）。
    /// `process_h3_events` がダーティ集合を降ろすかどうかの判定に使う。
    fn flush_partial_responses(&mut self) -> io::Result<bool> {
        let h3_conn = match &mut self.h3_conn {
            Some(h3) => h3,
            None => return Ok(false),
        };

        let mut completed = Vec::new();
        let mut did_work = false;
        for (&stream_id, pr) in &mut self.partial_responses {
            // 進捗判定用に呼び出し前の状態を控える（try_flush_partial 自体は変更しない）。
            let written_before = pr.written;
            let head_present_before = pr.head.is_some();
            if Self::try_flush_partial(h3_conn, &mut self.conn, stream_id, pr) {
                completed.push(stream_id);
                did_work = true;
            } else if pr.written != written_before || pr.head.is_some() != head_present_before {
                // 未完了でもヘッダ送出/部分バイト送出があった＝進捗あり。
                did_work = true;
            }
        }
        for stream_id in completed {
            self.partial_responses.remove(&stream_id);
        }

        Ok(did_work)
    }

    /// 書き込み可能なストリームを処理（quiche パターン）
    ///
    /// conn.writable() で書き込み可能になったストリームに対して、
    /// 保留中の部分レスポンスを再送します。
    ///
    /// F-151: 戻り値は「1 件でも進捗があったか」。B-12 再発防止のため、
    /// ダーティ集合を降ろすかどうかの判定に使う（進捗があれば次イテレーションでも
    /// もう一度見る）。
    fn handle_writable_streams(&mut self) -> io::Result<bool> {
        let h3_conn = match &mut self.h3_conn {
            Some(h3) => h3,
            None => return Ok(false),
        };

        // 書き込み可能なストリームを収集
        let writable_streams: Vec<u64> = self.conn.writable().collect();

        let mut completed = Vec::new();
        let mut did_work = false;
        for stream_id in writable_streams {
            // 部分レスポンスがあるかチェック
            if let Some(pr) = self.partial_responses.get_mut(&stream_id) {
                let written_before = pr.written;
                let head_present_before = pr.head.is_some();
                if Self::try_flush_partial(h3_conn, &mut self.conn, stream_id, pr) {
                    completed.push(stream_id);
                    did_work = true;
                } else if pr.written != written_before || pr.head.is_some() != head_present_before {
                    did_work = true;
                }
            }
        }
        for stream_id in completed {
            self.partial_responses.remove(&stream_id);
        }

        Ok(did_work)
    }
}

// ====================
// F-32: ストリーミング用フリー関数（リクエスト head 構築・パス計算・ストリーム駆動）
// ====================

/// プレフィックス除去 + `path_prefix` 連結でバックエンドへ送るパスを構築する。
/// `handle_proxy` と同一ロジック（挙動を一致させるため共有）。
fn compute_backend_path(target: &ProxyTarget, req_path: &[u8], prefix: &[u8]) -> String {
    let path_str = std::str::from_utf8(req_path).unwrap_or("/");
    compute_upstream_request_path(path_str, prefix, &target.path_prefix, false)
}

/// 上流リクエストパスを構築する。
///
/// - `preserve_full_path = true`（gRPC）: ルート `/*` プレフィックスを除去せずフルパスを返す（B-39）
/// - それ以外: `prefix` を剥がし `target_path_prefix` を前置
fn compute_upstream_request_path(
    path_str: &str,
    prefix: &[u8],
    target_path_prefix: &str,
    preserve_full_path: bool,
) -> String {
    if preserve_full_path {
        return if path_str.is_empty() {
            "/".to_string()
        } else {
            path_str.to_string()
        };
    }

    let sub_path = if prefix.is_empty() {
        path_str.to_string()
    } else {
        let prefix_str = std::str::from_utf8(prefix).unwrap_or("");
        if let Some(remaining) = path_str.strip_prefix(prefix_str) {
            let base = target_path_prefix.trim_end_matches('/');
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

/// リクエストヘッダが gRPC（`application/grpc*`）かどうかを判定する。
#[cfg(feature = "grpc")]
fn header_pairs_indicate_grpc(headers: &[(Vec<u8>, Vec<u8>)]) -> bool {
    headers.iter().any(|(n, v)| {
        n.eq_ignore_ascii_case(b"content-type") && crate::grpc::headers::is_grpc_content_type(v)
    })
}

/// gRPC over H3 の初期応答ヘッダから、trailers / 擬似ヘッダ / 重複を除外する（B-41）。
///
/// `grpc-status` / `grpc-message` は `send_additional_headers` の trailers 専用にし、
/// 初期 HEADERS には載せない。
#[cfg(feature = "grpc")]
fn filter_h3_grpc_initial_headers<'a>(
    headers: &'a [(&'a [u8], &'a [u8])],
) -> impl Iterator<Item = (&'a [u8], &'a [u8])> + 'a {
    headers.iter().copied().filter(|(name, _)| {
        *name != b":status"
            && !name.eq_ignore_ascii_case(b"content-type")
            && !name.eq_ignore_ascii_case(b"grpc-status")
            && !name.eq_ignore_ascii_case(b"grpc-message")
            && !name.eq_ignore_ascii_case(b"content-length")
    })
}

/// ホップバイホップヘッダを除き、必要なら CL/CE を除いたうえで trailers をマージする（B-39）。
fn merge_response_headers_and_trailers(
    headers: &[(Vec<u8>, Vec<u8>)],
    trailers: &[(Vec<u8>, Vec<u8>)],
    skip_content_length_encoding: bool,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut owned: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(headers.len() + trailers.len());
    for (name, value) in headers {
        if name.eq_ignore_ascii_case(b"connection")
            || name.eq_ignore_ascii_case(b"transfer-encoding")
            || name.eq_ignore_ascii_case(b"keep-alive")
        {
            continue;
        }
        if skip_content_length_encoding
            && (name.eq_ignore_ascii_case(b"content-length")
                || name.eq_ignore_ascii_case(b"content-encoding"))
        {
            continue;
        }
        owned.push((name.clone(), value.clone()));
    }
    for (name, value) in trailers {
        if !owned.iter().any(|(n, _)| n.eq_ignore_ascii_case(name)) {
            owned.push((name.clone(), value.clone()));
        }
    }
    owned
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

/// 1 ストリームの駆動結果。`(did_work, done)`。`did_work` は F-151 のダーティ集合維持判定
/// （進捗があれば呼び出し側がダーティのまま再投入する）、`done` が `true` なら呼び出し側が
/// `proxy_streams` から除去する。
fn drive_proxy_stream(
    h3: &mut h3::Connection,
    conn: &mut quiche::Connection,
    stream_id: u64,
    ps: &mut ProxyStream,
) -> (bool, bool) {
    let pump_work = drive_request_pump(h3, conn, stream_id, ps);
    let flush_work = drive_response_flush(h3, conn, stream_id, ps);
    // 完了条件: レスポンス fin 送出済み かつ リクエスト側クローズ済み。
    let done = ps.resp_fin_sent && ps.req_tx.is_none() && ps.req_pending.is_empty();
    (pump_work || flush_work, done)
}

/// リクエストボディ pump: `recv_body` → req チャネル（フロー制御 + バックプレッシャ）。
///
/// F-151: 戻り値は「1 件でも進捗があったか」。フロー制御で完全にブロックされた
/// （`Full`/`is_full`）だけの呼び出しは `false`（進捗なし）とし、ダーティ集合からの
/// ビジーループ再投入を避ける。それ以外の状態遷移（送出・EOF 検出・破棄）は `true` とし、
/// B-12 の不変条件（イベントが残っているのに誰も見なくなる経路を作らない）を安全側に倒す。
fn drive_request_pump(
    h3: &mut h3::Connection,
    conn: &mut quiche::Connection,
    stream_id: u64,
    ps: &mut ProxyStream,
) -> bool {
    use crate::http3_stream::TrySendError;
    let mut did_work = false;
    // バックエンドタスクが終了して本文が不要になった（チャネルが満杯のまま受信側が
    // 閉じた場合も含む。満杯だと try_send に到達せず Closed を観測できない）。
    if ps.req_tx.as_ref().is_some_and(|t| t.is_closed()) && !ps.req_too_large {
        abandon_request_body(conn, stream_id, ps);
        return true;
    }
    let tx = match &ps.req_tx {
        Some(t) => t,
        None => return did_work,
    };

    // ボディ上限超過済みなら何もしない（応答 flush 側で 413 + リセット）。
    if ps.req_too_large {
        return did_work;
    }

    // 1. 未投入ボディ（初回バッチ/溢れ分）を先に流す（ゼロコピー: clone せず move）。
    while let Some(front) = ps.req_pending.pop_front() {
        match tx.try_send(front) {
            Ok(()) => did_work = true,
            Err(TrySendError::Full(item)) => {
                ps.req_pending.push_front(item); // バックプレッシャ: recv_body も止める。
                return did_work;
            }
            Err(TrySendError::Closed(_)) => {
                abandon_request_body(conn, stream_id, ps);
                return true;
            }
        }
    }

    // 2. quiche から recv_body してチャネルへ（容量がある間だけ = バックプレッシャ）。
    if ps.req_readable {
        loop {
            if tx.is_full() {
                return did_work; // データを quiche に残す → フロー制御でクライアント送信が止まる。
            }
            let mut buf = BytesMut::with_capacity(REQ_RECV_CHUNK);
            let spare = buf.spare_capacity_mut();
            // SAFETY: recv_body は read バイトのみ初期化。advance_mut で len に反映。
            let spare_u8 = unsafe {
                std::slice::from_raw_parts_mut(spare.as_mut_ptr() as *mut u8, spare.len())
            };
            match h3.recv_body(conn, stream_id, spare_u8) {
                Ok(n) if n > 0 => {
                    did_work = true;
                    unsafe { buf.advance_mut(n) };
                    ps.req_bytes_total += n as u64;
                    // ボディ上限チェック（0 = 無制限）。
                    if ps.max_request_body > 0 && ps.req_bytes_total > ps.max_request_body {
                        ps.req_too_large = true;
                        ps.req_tx = None; // バックエンドタスクを中断。
                        ps.req_pending.clear();
                        // クライアントの送信を止める。
                        let _ = conn.stream_shutdown(stream_id, quiche::Shutdown::Read, 0);
                        return true;
                    }
                    match tx.try_send(buf.freeze()) {
                        Ok(()) => continue,
                        Err(TrySendError::Full(b)) => {
                            ps.req_pending.push_back(b);
                            return did_work;
                        }
                        Err(TrySendError::Closed(_)) => {
                            abandon_request_body(conn, stream_id, ps);
                            return true;
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
        did_work = true; // 新規に EOF を検出 = 進捗（response flush 側が最終処理できるように）。
    }

    // 3. クライアント END_STREAM 受信かつ全消化なら送信端を閉じて EOF 伝播。
    if ps.req_eof_seen && ps.req_pending.is_empty() && !ps.req_readable {
        ps.req_tx = None;
        did_work = true;
    }

    did_work
}

/// バックエンドタスクが要求本文を必要としなくなった（早期応答・バックエンド切断で
/// req チャネルの受信側が閉じた）とき、要求ストリームの受信を打ち切る。
///
/// 以前は `req_tx` を落とすだけで `recv_body` を呼ばなくなっていたため、未受信の本文が
/// quiche に溜まって QUIC のフロー制御ウィンドウが補充されず、**本文を送り切ってから応答を
/// 読むクライアントは送信が止まったまま応答を受け取れず、アイドルタイムアウトまで停止した**
/// （B-68 の 30 秒待ちの直接の原因）。RFC 9114 §4.1.1 に従い `STOP_SENDING(H3_NO_ERROR)` で
/// 送信停止を求める（quiche は以後の受信データを破棄し、クライアントは応答を読める）。
/// クライアントが既に本文を送り切っている（fin 受信済み）場合は何もしない。
fn abandon_request_body(conn: &mut quiche::Connection, stream_id: u64, ps: &mut ProxyStream) {
    /// RFC 9114 §8.1 H3_NO_ERROR
    const H3_NO_ERROR: u64 = 0x100;
    ps.req_pending.clear();
    ps.req_tx = None;
    if !ps.req_eof_seen && !conn.stream_finished(stream_id) {
        let _ = conn.stream_shutdown(stream_id, quiche::Shutdown::Read, H3_NO_ERROR);
    }
}

/// レスポンス flush: resp チャネル → `send_response`/`send_body`（フロー制御 + 部分送信保持）。
///
/// F-151: 戻り値は「1 件でも進捗があったか」（ヘッダ/ボディ/fin を実際に送出できた、または
/// エラー応答で完了させた）。`Blocked`（quiche 側フロー制御で 0 バイトも送れなかった）は
/// 進捗なしとして扱い、ビジーループ再投入を避ける。
fn drive_response_flush(
    h3: &mut h3::Connection,
    conn: &mut quiche::Connection,
    stream_id: u64,
    ps: &mut ProxyStream,
) -> bool {
    use crate::http3_stream::{RespMsg, TryRecv};

    if ps.resp_fin_sent {
        return false;
    }

    let mut did_work = false;

    // ボディ上限超過 → 413 を返して終了（応答未開始時のみ）。
    if ps.req_too_large && !ps.resp_started {
        send_simple_h3_error(h3, conn, stream_id, 413);
        ps.resp_fin_sent = true;
        return true;
    }

    // 0. 保留中の fin を再送。
    if ps.need_fin {
        if try_send_h3_fin(h3, conn, stream_id) {
            ps.resp_fin_sent = true;
            ps.need_fin = false;
            did_work = true;
        }
        return did_work;
    }

    // 1. StreamBlocked で保留した head を再送。
    if let Some((status, headers)) = ps.head_pending.take() {
        match send_h3_head(h3, conn, stream_id, status, &headers) {
            HeadSend::Sent => {
                ps.resp_started = true;
                did_work = true;
            }
            HeadSend::Blocked => {
                ps.head_pending = Some((status, headers));
                return did_work;
            }
            HeadSend::Error => {
                ps.resp_fin_sent = true;
                return true;
            }
        }
    }

    // 2. 部分送信のボディ断片を flush。
    if let Some((buf, off)) = ps.body_pending.take() {
        match send_h3_body(h3, conn, stream_id, &buf, off) {
            BodySend::Done => did_work = true,
            BodySend::Partial(new_off) => {
                did_work = true; // 一部でも送れたので進捗あり。
                ps.body_pending = Some((buf, new_off));
                return did_work;
            }
            BodySend::Blocked => {
                ps.body_pending = Some((buf, off));
                return did_work;
            }
            BodySend::Error => {
                ps.resp_fin_sent = true;
                return true;
            }
        }
    }

    // 3. チャネルを排出して送出。
    loop {
        if ps.head_pending.is_some() || ps.body_pending.is_some() {
            return did_work;
        }
        match ps.resp_rx.try_recv() {
            TryRecv::Item(RespMsg::Head { status, headers }) => {
                match send_h3_head(h3, conn, stream_id, status, &headers) {
                    HeadSend::Sent => {
                        ps.resp_started = true;
                        did_work = true;
                    }
                    HeadSend::Blocked => {
                        ps.head_pending = Some((status, headers));
                        return did_work;
                    }
                    HeadSend::Error => {
                        ps.resp_fin_sent = true;
                        return true;
                    }
                }
            }
            TryRecv::Item(RespMsg::Body(b)) => match send_h3_body(h3, conn, stream_id, &b, 0) {
                BodySend::Done => did_work = true,
                BodySend::Partial(off) => {
                    did_work = true;
                    ps.body_pending = Some((b, off));
                    return did_work;
                }
                BodySend::Blocked => {
                    ps.body_pending = Some((b, 0));
                    return did_work;
                }
                BodySend::Error => {
                    ps.resp_fin_sent = true;
                    return true;
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
                return true;
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
                return true;
            }
            TryRecv::Empty => return did_work,
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
    /// HTTP/2 trailers（H2C/gRPC 用。H1 バックエンドでは空）
    pub trailers: Vec<(Vec<u8>, Vec<u8>)>,
}

pub(crate) async fn proxy_to_backend_async_with_tls(
    target: &ProxyTarget,
    request: Vec<u8>,
    timeout_secs: u64,
    tls_insecure: bool,
) -> io::Result<BackendProxyResult> {
    use crate::runtime::handle::AsRawFd;
    use crate::runtime::tcp::TcpStream;

    // F-170: 接続先表記（UDS 対応、TCP は不変）。この経路は非同期 `connect_str` を
    // 使うため `unix:` 接頭辞をそのまま扱える。
    let addr = target.conn_addr();
    let addr = addr.as_str();
    debug!("[HTTP/3] Async connecting to backend {}", addr);

    // 非同期TCP接続（タイムアウト付き）
    let connect_future = TcpStream::connect_str(addr);
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
#[cfg(veil_ktls)]
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
    // F-170: 接続先表記（UDS 対応、TCP は不変）。別スレッドへ move するため所有文字列化する。
    let addr = target.conn_addr().as_str().to_string();
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
                crate::tls_provider::provider::default_provider()
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
            // F-170: TCP/UDS 共通の接続入口（`upstream::connect_probe` を再利用し、
            // 同じ列挙を重複実装しない）。
            let mut std_stream = crate::upstream::connect_probe(&addr, timeout).map_err(|e| {
                warn!("[HTTP/3] std backend connect error: {}", e);
                e
            })?;
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
#[cfg(not(veil_ktls))]
// 理由付き allow: 同期 connect/TLS は std::thread::spawn した専用スレッド内で実行し、結果を mpsc + ポーリングで受け取る（イベントループ非ブロック）。
#[allow(clippy::disallowed_methods)]
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
    // F-170: 接続先表記（UDS 対応、TCP は不変）。別スレッドへ move するため所有文字列化する。
    let addr = target.conn_addr().as_str().to_string();
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
                crate::tls_provider::provider::default_provider()
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
            // F-170: TCP/UDS 共通の接続入口（`upstream::connect_probe` を再利用し、
            // 同じ列挙を重複実装しない）。
            let mut std_stream = crate::upstream::connect_probe(&addr, timeout).map_err(|e| {
                warn!("[HTTP/3] std backend connect error: {}", e);
                e
            })?;
            let server_name = rustls::pki_types::ServerName::try_from(sni_name)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
            let mut conn = rustls::ClientConnection::new(config, server_name)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            let mut tls = rustls::Stream::new(&mut conn, &mut std_stream);
            tls.write_all(&request)?;
            let mut response = Vec::with_capacity(16384);
            // `read_to_end` は rustls の «close_notify なしの EOF» をエラーとして
            // 伝播させてしまい、HTTP/3 → TLS バックエンドのプロキシが 502 になる
            // （B-54）。close_notify を送らずに閉じるバックエンドは HTTP/1.1 では
            // ごく普通なので、kTLS 版と同じく UnexpectedEof は正常終了として扱う。
            let mut buf = [0u8; 8192];
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

#[inline]
fn read_nonblocking(fd: crate::runtime::handle::RawFd, buf: &mut [u8]) -> io::Result<usize> {
    let result = unsafe {
        libc::read(
            fd as libc::c_int,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len() as _,
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

#[inline]
fn write_nonblocking(fd: crate::runtime::handle::RawFd, buf: &[u8]) -> io::Result<usize> {
    let result = unsafe {
        libc::write(
            fd as libc::c_int,
            buf.as_ptr() as *const libc::c_void,
            buf.len() as _,
        )
    };
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
        trailers: Vec::new(),
    })
}

/// B-38: HTTP/3 経路で WASM on_response_headers を適用する
#[cfg(feature = "wasm")]
async fn apply_h3_wasm_response_headers(
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
async fn finish_h3_wasm_lifecycle(
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

/// B-39/B-74: `proxy_to_h2c_backend_async` 用に新規 TCP 接続 + H2C ハンドシェイクを行う。
///
/// プールミス時の新規接続と、プール接続での送信失敗時の再接続の両方から共有する。
#[cfg(feature = "http2")]
async fn h3_h2c_connect_and_handshake(
    addr: &str,
    timeout_secs: u64,
) -> io::Result<crate::http2::H2cClient<crate::runtime::tcp::TcpStream>> {
    use crate::http2::{H2cClient, Http2Settings};
    use crate::runtime::tcp::TcpStream;

    debug!("[HTTP/3] H2C connecting to backend {}", addr);

    let connect_future = TcpStream::connect_str(addr);
    let backend = match crate::runtime::time::timeout(
        Duration::from_secs(timeout_secs),
        connect_future,
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            warn!("[HTTP/3] H2C backend connect error: {}", e);
            return Err(e);
        }
        Err(_) => {
            warn!("[HTTP/3] H2C backend connect timeout");
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "H2C backend connect timeout",
            ));
        }
    };
    let _ = backend.set_nodelay(true);

    let settings = Http2Settings::default();
    let mut client = H2cClient::new(backend, settings);

    if let Err(e) = client.handshake().await {
        warn!("[HTTP/3] H2C handshake error: {}", e);
        return Err(io::Error::other(format!("H2C handshake: {}", e)));
    }

    Ok(client)
}

/// B-39: HTTP/3 → H2C 上流プロキシ（gRPC 等）
///
/// Prior Knowledge で H2C 接続し、レスポンスヘッダ + ボディ + trailers を返す。
///
/// B-74: `proxy.rs` の `h2_proxy_h2c`（HTTP/2 経路）と同様、`crate::pool::H2C_POOL`
/// （スレッドローカル）で上流 H2C 接続を再利用する。HTTP/3 のワーカースレッドと
/// HTTP/2 のワーカースレッドは別スレッドであり、`H2C_POOL` はスレッドローカルなので
/// 相互に干渉しない（同一スレッド内での再利用のみ）。プールが無かった旧実装は
/// リクエストごとに TCP 接続 + ハンドシェイクを行い、高負荷時にエフェメラルポートを
/// 枯渇させて `EADDRNOTAVAIL` を引き起こしていた。
#[cfg(feature = "http2")]
async fn proxy_to_h2c_backend_async(
    target: &ProxyTarget,
    method: &[u8],
    path: &[u8],
    headers: &[(Vec<u8>, Vec<u8>)],
    request_body: &[u8],
    timeout_secs: u64,
    security: &SecurityConfig,
) -> io::Result<BackendProxyResult> {
    // F-41/B-74/F-170: リクエストごとの `format!("{host}:{port}")` ヒープ確保をスタック
    // 整形で排除しつつ、UDS バックエンド（unix:<path>）にも対応する。
    let addr = target.conn_addr();
    let addr = addr.as_str();

    let from_pool;
    let mut client = match crate::pool::H2C_POOL.with(|p| p.borrow_mut().get(addr)) {
        Some(c) => {
            from_pool = true;
            c
        }
        None => {
            from_pool = false;
            h3_h2c_connect_and_handshake(addr, timeout_secs).await?
        }
    };

    let body = if request_body.is_empty() {
        None
    } else {
        Some(request_body)
    };
    let authority = target.host.as_bytes();
    // F-166/F-165(A2): 中間 `Vec<(&[u8], &[u8])>` を作らずイテレータを直接渡す
    // （送信失敗時の再試行のため、同じフィルタ済みイテレータをクロージャで再構築する）。
    let headers_iter = || headers.iter().map(|(k, v)| (k.as_slice(), v.as_slice()));

    let mut response = crate::runtime::time::timeout(
        Duration::from_secs(timeout_secs),
        client.send_request(method, path, authority, headers_iter(), body),
    )
    .await;

    // プール由来の接続は上流に既に切られている可能性がある（B-74）。送信が
    // 失敗（エラー/タイムアウトいずれも）した場合、新規接続で 1 回だけ再試行する。
    if from_pool && !matches!(response, Ok(Ok(_))) {
        if let Ok(fresh) = h3_h2c_connect_and_handshake(addr, timeout_secs).await {
            client = fresh;
            response = crate::runtime::time::timeout(
                Duration::from_secs(timeout_secs),
                client.send_request(method, path, authority, headers_iter(), body),
            )
            .await;
        }
    }

    let response = match response {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => {
            warn!("[HTTP/3] H2C request error: {}", e);
            return Err(io::Error::other(format!("H2C request: {}", e)));
        }
        Err(_) => {
            warn!("[HTTP/3] H2C request timeout");
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "H2C request timeout",
            ));
        }
    };

    // 応答が取得できた接続は、再利用可能ならプールへ返却する。
    if client.is_reusable() {
        let max_idle = security.max_idle_connections_per_host;
        let idle_timeout = security.idle_connection_timeout_secs;
        crate::pool::H2C_POOL.with(|p| p.borrow_mut().put(addr, client, max_idle, idle_timeout));
    }

    debug!(
        "[HTTP/3] H2C response: status={} body_len={} trailers={}",
        response.status,
        response.body.len(),
        response.trailers.len()
    );

    Ok(BackendProxyResult {
        status_code: response.status,
        // `H2cResponse` は F-166/F-165(A4) で `Bytes` 化されている。`BackendProxyResult`
        // は本タスクの対象範囲外（HTTP/3 経路）のため型は変えず、境界で `Vec<u8>` へ
        // 変換する（`Bytes` は一意参照なら `Vec::from` がコピー無しで引き取る）。
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
static HTTP3_LOG_ONCE_CERT_LOADING: Once = Once::new();
static HTTP3_LOG_ONCE_CERT_LOADED: Once = Once::new();
static HTTP3_LOG_ONCE_TRANSPORT: Once = Once::new();
static HTTP3_LOG_ONCE_GSO_GRO: Once = Once::new();
static HTTP3_LOG_ONCE_LISTENING: Once = Once::new();
// パイプライン化 io_uring RECVMSG/SENDMSG のログは Linux uring バックエンドでのみ
// 存在する分岐（`#[cfg(all(target_os = "linux", veil_rt_uring))]`）の中でのみ使うため、
// 他バックエンド（reactor/epoll）ビルドで未使用 static にならないよう同じ cfg を付ける。
#[cfg(all(target_os = "linux", veil_rt_uring))]
static HTTP3_LOG_ONCE_RECVMSG: Once = Once::new();
#[cfg(all(target_os = "linux", veil_rt_uring))]
static HTTP3_LOG_ONCE_SENDMSG: Once = Once::new();

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

    // F-149(B.9): monoio は F-28 で除去済みで現存しない。ビルドバックエンドに応じて実態の
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
    let connections: ConnectionMap = Rc::new(RefCell::new(HashMap::new()));

    // F-32: バックエンドタスク → メインループの起床通知（全ハンドラ/タスクで共有）。
    let notify = crate::http3_stream::H3Notify::new();
    // F-46: バックエンドタスクの型付きプール（本ワーカースレッドの全接続で共有）。
    let backend_spawner = crate::http3_stream::backend_task_spawner();

    // 乱数生成器
    let rng = SystemRandom::new();

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
    let mut timers: BinaryHeap<Reverse<TimerKey>> = BinaryHeap::new();
    // F-151: バックエンドタスクが per-connection に積む起床キュー（`Http3Handler` 生成時に
    // `ConnWaker` へ `Rc` で共有する）。
    let wake_queue: crate::http3_stream::WakeQueue = Rc::new(RefCell::new(VecDeque::new()));
    // F-151: 末尾の送出スイープで走査する対象 cid（このイテレーションで処理した接続）。
    // ループ外で 1 度だけ確保し、毎イテレーション clear して使い回す。
    let mut send_targets: Vec<ConnKey> = Vec::new();

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
                    _ = futures::FutureExt::fuse(notify.wait()) => MsOutcome::Notified,
                    _ = futures::FutureExt::fuse(crate::runtime::time::sleep(timeout_duration)) => MsOutcome::Timeout,
                };

                // F-151: タイマーヒープから期限到来分だけ pop して on_timeout する。
                {
                    let mut conns = connections.borrow_mut();
                    expire_due_timers(&mut conns, &mut timers, &mut dirty_queue, Instant::now());
                    drain_wake_queue(&wake_queue, &mut conns, &mut dirty_queue);
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
                                            &rng,
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
                RecvOutcome::Notified | RecvOutcome::Timeout => None,
            };

            if let Some(first_gro) = gro_result {
                let mut conns = connections.borrow_mut();
                let first_total = first_gro.bytes_received;
                process_datagram_segments(
                    &mut conns,
                    &mut recv_buf[..first_total],
                    first_gro.from,
                    first_gro.gro_segment_size,
                    &rng,
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
                            &rng,
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
                    match handler.process_h3_events().await {
                        Ok(w) => did_work |= w,
                        Err(e) => warn!("[HTTP/3] process_h3_events error: {}", e),
                    }
                }

                // F-32: ストリーミングストリームを駆動。バックエンドタスクが生成したレスポンス
                // 断片を send_body、recv_body したリクエストボディをチャネルへ流す。
                if handler.h3_conn.is_some() {
                    did_work |= handler.drive_proxy_streams();
                }

                // タイマー再登録（コネクション処理直後）。
                schedule_timer(handler, &cid, Instant::now(), &mut timers);

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

// ====================
// F-151: ダーティ接続集合 + タイマー最小ヒープ（イベント駆動化）
// ====================

/// タイマーヒープの pop が来ない場合のフォールバック値であり、かつ `select` の sleep 時間の
/// 上限クランプにも使う（従来のタイムアウト粒度 100ms から挙動を変えない）。
const H3_DEFAULT_TIMER: Duration = Duration::from_millis(100);

/// 接続 ID の共有ハンドル（`ConnKey` = `Rc<ConnectionId<'static>>`）。ダーティキュー/
/// タイマーヒープ/送出対象リストはすべてこの型を使い、`clone()` を `Rc::clone`
/// （参照カウント +1 のみ）に落として `ConnectionId` 本体のヒープ確保を避ける
/// （F-151 レビュー修正。定義本体は [`crate::http3_stream::ConnKey`]）。
type ConnKey = crate::http3_stream::ConnKey;

/// タイマーヒープ（`BinaryHeap<Reverse<TimerKey>>`）のエントリ。
///
/// `quiche::ConnectionId` は `Ord`/`PartialOrd` を実装していないため `(Instant,
/// ConnectionId)` タプルをそのまま `BinaryHeap` の要素にはできない。本型は `Instant` のみで
/// 順序付けし（同着時の順序は問わない。タイマー期限の到来判定にしか使わないため cid の
/// 大小関係は無関係）、`ConnKey` は識別子として運ぶだけにする。
struct TimerKey(Instant, ConnKey);

impl PartialEq for TimerKey {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl Eq for TimerKey {}
impl PartialOrd for TimerKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for TimerKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

/// ヒープ先頭の期限から次の `select` sleep 時間を算出する（quiche/`Http3Handler` 非依存の
/// 純粋関数。単体テスト対象）。
///
/// - **`has_dirty = true`（ダーティ集合に未処理の接続が残っている）→ 常に 0。**
///   前イテレーションで `did_work` により再投入された接続がある状態で sleep してしまうと、
///   ストリーミング中のレスポンスがチャンクごとに最大 `H3_DEFAULT_TIMER` 停止し得る
///   （レイテンシ退行。F-151 レビュー修正）。
/// - ヒープが空（`top = None`）→ 既定 100ms。
/// - 先頭期限が既に過去（`top <= now`）→ 0（即座に処理すべきタイマーがある）。
/// - 先頭期限が未来 → その差分。ただし上限を 100ms にクランプし、従来のタイムアウト粒度
///   から外れないようにする。
fn next_sleep_duration(top: Option<Instant>, now: Instant, has_dirty: bool) -> Duration {
    if has_dirty {
        return Duration::ZERO;
    }
    match top {
        None => H3_DEFAULT_TIMER,
        Some(dl) if dl <= now => Duration::ZERO,
        Some(dl) => (dl - now).min(H3_DEFAULT_TIMER),
    }
}

/// タイマーヒープの遅延削除判定（quiche/`Http3Handler` 非依存の純粋関数。単体テスト対象）。
///
/// pop したエントリの期限 `popped_deadline` が、そのハンドラの現在の期限
/// （`Http3Handler::timer_deadline` 相当）と一致する場合のみ有効。期限が更新された後の
/// 古いエントリは `false` を返し、呼び出し側は無視して捨てる。
fn timer_entry_is_current(popped_deadline: Instant, current_deadline: Option<Instant>) -> bool {
    current_deadline == Some(popped_deadline)
}

/// ダーティフラグ管理の核（quiche/`Http3Handler` 非依存。単体テスト対象）。
///
/// 既にダーティ（`*dirty == true`）なら何もしない（多重登録防止）。そうでなければ
/// ダーティにしてキューへ push する。戻り値は「実際に push したか」。
fn mark_dirty_flag<T: Clone>(dirty: &mut bool, queue: &mut VecDeque<T>, id: &T) -> bool {
    if *dirty {
        false
    } else {
        *dirty = true;
        queue.push_back(id.clone());
        true
    }
}

/// 接続をダーティ集合へ登録する（多重登録防止）。`Http3Handler::dirty` が `false` のときだけ
/// `true` にしてキューへ push する。
///
/// 呼び出し元が既に対象 `Http3Handler` を可変借用している場合はこの関数を使わず、
/// `mark_dirty_flag(&mut handler.dirty, queue, key)` を直接呼ぶ（`conns` の二重借用を避ける
/// ため。`process_datagram_segments` の受信直後の呼び出し箇所を参照）。
fn mark_dirty(
    conns: &mut HashMap<ConnectionId<'static>, Http3Handler>,
    queue: &mut VecDeque<ConnKey>,
    key: &ConnKey,
) {
    // `ConnKey` = `Rc<ConnectionId>` を `&**key` で deref し、HashMap キー（素の
    // `ConnectionId<'static>`）としてルックアップする。
    if let Some(h) = conns.get_mut(&**key) {
        mark_dirty_flag(&mut h.dirty, queue, key);
    }
}

/// コネクション処理直後にタイマーヒープへ次回期限を登録する（遅延削除方式）。
///
/// `conn.timeout()` が `None`（アイドル/未確立等）の場合は `H3_DEFAULT_TIMER` をフォール
/// バックとして使う。
fn schedule_timer(
    handler: &mut Http3Handler,
    key: &ConnKey,
    now: Instant,
    timers: &mut BinaryHeap<Reverse<TimerKey>>,
) {
    let deadline = now + handler.conn.timeout().unwrap_or(H3_DEFAULT_TIMER);
    handler.timer_deadline = Some(deadline);
    timers.push(Reverse(TimerKey(deadline, key.clone())));
}

/// タイマーヒープから期限到来分だけ pop して `on_timeout` を呼び、ダーティ化する（F-151）。
///
/// 全接続の `conn.timeout()` の最小値を求める走査と、期限が来ていない接続まで
/// `on_timeout` する走査（従来方式）を廃止する。遅延削除方式: pop したエントリの期限が
/// 現在の `timer_deadline` と一致しなければ（期限が更新済みの古いエントリ）無視して捨てる。
fn expire_due_timers(
    conns: &mut HashMap<ConnectionId<'static>, Http3Handler>,
    timers: &mut BinaryHeap<Reverse<TimerKey>>,
    dirty_queue: &mut VecDeque<ConnKey>,
    now: Instant,
) {
    while let Some(top) = timers.peek() {
        if top.0 .0 > now {
            break;
        }
        // peek で存在確認済みなので pop は必ず成功する。
        let Reverse(TimerKey(dl, key)) = timers.pop().expect("peek succeeded");

        let Some(handler) = conns.get_mut(&*key) else {
            continue; // 接続は既に削除済み。
        };
        if !timer_entry_is_current(dl, handler.timer_deadline) {
            continue; // 期限が更新済みの古いエントリ（遅延削除）。
        }
        handler.conn.on_timeout();
        if handler.conn.is_closed() {
            debug!("[HTTP/3] Connection closed (timeout)");
            conns.remove(&*key);
            continue;
        }
        mark_dirty(conns, dirty_queue, &key);
    }
}

/// バックエンド通知キュー（`ConnWaker` が積む cid）を drain してダーティ化する（F-151）。
///
/// per-connection 化により「どの接続が進んだか」を追跡できるため、全接続をダーティ化する
/// フォールバックは不要（本関数はキューに積まれた cid だけを処理する）。
///
/// F-161: 要素は `(cid, queued フラグ)`。**pop → flag=false → mark_dirty** の順で処理する。
/// flag を先に false に戻してから mark_dirty するため、この間（mark_dirty 実行中も含む）に
/// バックエンドタスクが新たに notify() しても「flag は false → push される」ため
/// 取りこぼされず、次の drain で確実に処理される。逆に flag を mark_dirty の後で false に
/// 戻す順序にすると、mark_dirty 実行後・flag=false 前に発生した notify() が
/// 「flag はまだ true → push されない」まま消えてしまう（取りこぼし）。
fn drain_wake_queue(
    wake_queue: &crate::http3_stream::WakeQueue,
    conns: &mut HashMap<ConnectionId<'static>, Http3Handler>,
    dirty_queue: &mut VecDeque<ConnKey>,
) {
    // borrow は drain 中だけ（`mark_dirty` は `wake_queue` に触れないため二重借用にならない）。
    let mut q = wake_queue.borrow_mut();
    while let Some((key, queued)) = q.pop_front() {
        queued.set(false);
        mark_dirty(conns, dirty_queue, &key);
    }
}

/// F-115 第2段: 受信した 1 データグラム（GRO 集約含む）を quiche へ供給する。
///
/// select 経由の最初の 1 通と recvmmsg 経由の各通で共用する（挙動を両経路で一致させるため
/// クロージャではなく専用 fn へ抽出）。`data` は受信済みデータグラム全体（長さ = 受信バイト数）で、
/// GRO 適用時は `gro_segment_size` 境界で複数 QUIC パケットへ分割される。
///
/// ゼロコピー: `data` のスライスを `quiche::Header::from_slice` と `conn.recv` に直接渡し、
/// 中間 Vec へのコピーを一切行わない。
///
/// F-45: GRO バッチはカーネルが**同一フロー**のデータグラムを集約したものなので、直前セグメントと
/// 同じ DCID なら新規接続判定（contains_key + Initial 検査）をスキップし、per-segment の
/// オーバーヘッドをルックアップ 1 回に抑える（`prev_cid` 最適化）。quiche の `recv` API は
/// 1 データグラム単位のため呼び出し自体は per-segment。
///
/// F-151: そのコネクションに `conn.recv()` した接続（新規コネクション生成時も含む）は
/// 必ずダーティ化する。これがダーティ化の契機の 1 つ（他の契機はタイマー期限到来・
/// バックエンド起床キュー・前回パスの仕事継続）。
///
/// **タイマーの再登録はここでは行わない**（レビュー修正）。本関数は 1 データグラムごとに
/// 呼ばれるホットパスであり、GRO で束ねられた 54KB のレスポンス相当では 1 リクエストあたり
/// 40 回以上呼ばれ得る。ここで `schedule_timer` を呼ぶと呼び出しのたびに `ConnKey` の
/// ヒープ確保（`Rc::new` 相当）は避けられても `BinaryHeap::push` が積み重なり、F-151 で
/// 削減したかった固定費を上回りかねない。recv した接続は必ずダーティ化されるため、
/// **同一イテレーション内のダーティ処理ループ**（呼び出し元のメインループ）で
/// `schedule_timer` が接続ごとに高々 1 回だけ呼ばれ、タイマーの再登録は漏れなく行われる。
#[allow(clippy::too_many_arguments)] // F-151: ダーティ集合/起床キューの受け渡しで増加（ホットパスの単一呼び出し経路）
fn process_datagram_segments(
    conns: &mut HashMap<ConnectionId<'static>, Http3Handler>,
    data: &mut [u8],
    from: SocketAddr,
    gro_segment_size: Option<u16>,
    rng: &SystemRandom,
    quic_config: &Rc<RefCell<quiche::Config>>,
    local_addr: SocketAddr,
    notify: &crate::http3_stream::H3Notify,
    backend_spawner: &crate::http3_stream::BackendSpawner,
    wake_queue: &crate::http3_stream::WakeQueue,
    dirty_queue: &mut VecDeque<ConnKey>,
) -> io::Result<()> {
    let total = data.len();
    // GRO セグメントサイズ。None/0（GRO 非適用 = 単発データグラム）の場合は
    // 受信全体を 1 セグメントとして扱う。
    let seg_size = gro_segment_size
        .map(|s| s as usize)
        .filter(|&s| s > 0)
        .unwrap_or(total);

    let mut prev_cid: Option<ConnectionId<'static>> = None;
    let mut offset = 0;
    while offset < total {
        let start = offset;
        let end = (offset + seg_size).min(total);
        offset = end;

        // パケットヘッダーを解析（同一バッファスライスを後段の conn.recv にも渡す）
        let hdr = match quiche::Header::from_slice(&mut data[start..end], quiche::MAX_CONN_ID_LEN) {
            Ok(v) => v,
            Err(e) => {
                warn!("[HTTP/3] Invalid packet header: {}", e);
                // このセグメントのみスキップ。送信処理は呼び出し側ループ後に実行。
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
                    let conn = quiche::accept(&scid, None, local_addr, from, &mut config_ref)
                        .map_err(|e| io::Error::other(e.to_string()))?;

                    debug!("[HTTP/3] New connection from {}", from);

                    // F-151（レビュー修正）: 接続 ID の Rc ハンドルは接続の生成時に 1 度だけ
                    // 作る（以降はこの Rc を clone するだけで malloc なしに使い回せる）。
                    let key: ConnKey = Rc::new(scid.clone());
                    // この接続専用の ConnWaker を組み立てる（cid を積んでから notify() する
                    // per-connection 通知）。
                    let waker = crate::http3_stream::ConnWaker::new(
                        key.clone(),
                        wake_queue.clone(),
                        notify.clone(),
                    );
                    let mut handler =
                        Http3Handler::new(conn, from, waker, backend_spawner.clone(), key.clone());
                    // 新規接続は常にダーティ（後段の recv 直後のダーティ化と二重登録
                    // しないよう、挿入前に直接フラグを立てて自前でキューへ積む）。
                    handler.dirty = true;
                    conns.insert(scid.clone(), handler);
                    dirty_queue.push_back(key);

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

            match handler.conn.recv(&mut data[start..end], recv_info) {
                Ok(_) => {}
                Err(e) => {
                    warn!("[HTTP/3] recv error: {}", e);
                    // エラー時も送信処理は続行
                }
            }

            // B-34: クライアントがハンドシェイク直後に HTTP/3 フレームを送るため、
            // 次の recv やメインループ待ちの前に h3 レイヤを確立する（StreamLimit 回避）。
            if handler.conn.is_established() {
                if let Err(e) = handler.init_h3() {
                    warn!("[HTTP/3] eager init_h3 error: {}", e);
                }
            }

            // F-151: recv した接続は必ずダーティ化する（新規接続は既に dirty=true 済みで
            // ここではスキップされる）。`handler` を既に可変借用しているため、`mark_dirty`
            // （`&mut HashMap` を取る版）は使わずインラインでフラグを立てる。
            // `handler.key.clone()`（`Rc::clone`）は参照カウント +1 のみで malloc を伴わない
            // ため、`conn_id`（素の `ConnectionId`）を clone するより安価。
            if !handler.dirty {
                handler.dirty = true;
                dirty_queue.push_back(handler.key.clone());
            }
            // タイマーの再登録はここでは行わない（関数冒頭のコメント参照）。この接続は
            // 上でダーティ化されたので、呼び出し元のダーティ処理ループが
            // `schedule_timer` を呼ぶ。
        }
    }

    Ok(())
}

/// 保留中のパケットを対象コネクションに対して送信する。
///
/// この関数はメインループの各イテレーション末尾で常に 1 回だけ呼び出され、タイムアウト時でも
/// ACKやレスポンスパケットを送信します。
///
/// F-151: 走査対象は**全接続ではなく `targets`（このイテレーションでダーティ処理した接続）
/// だけ**に絞る。sendmmsg / io_uring SENDMSG のバッチ構築ロジック（`finalize_send_entry` /
/// `send_mmsg_flush` / GSO セグメント判定）は一切変更しない。
///
/// F-115 第2段: 従来は per-connection で GSO バッチを組み、**接続ごとに 1 回以上の
/// sendmsg/sendto** を発行していた（-c100 では 1 sweep 最大 ~100 syscall）。本実装は
/// sweep 中に送らずに送信要求を蓄積し（連結バッファ `batch` 内のパケット範囲を指す
/// `sends` インデックス列）、`MMSG_SEND_BATCH` 件到達 or `batch` バイト上限超過時点で
/// `sendmmsg` により **1 syscall = 複数メッセージ** で送出する。既存の flush 条件
/// （宛先変更 / セグメントサイズ不一致 / MAX_GSO_BATCH_BYTES / MAX_GSO_SEGMENTS /
/// 最終ショートセグメント）は「エントリ確定」に読み替える（即送信しない）。同一接続の
/// パケット順序は quiche の send 順（= batch 追記順）のまま保たれる（挙動不変）。
// clippy::await_holding_refcell_ref 許容理由: `connections`（Rc<RefCell<HashMap>>）を
// 借用するのは本 H3 メインループタスクのみ。バックエンドタスクは Rc チャネル + Notify
// 経由で通信し RefCell に触れない（F-32 のアクターモデル）ため、await 中に他タスクが
// 再入借用して panic する経路は存在しない（B-16 とは異なり単一借用者）。
// clippy::too_many_arguments 許容理由: F-151 でダーティ集合の受け渡しが増えた
// （ホットパスの単一呼び出し経路のため構造体化のオーバーヘッドを避ける）。
#[allow(clippy::await_holding_refcell_ref, clippy::too_many_arguments)]
async fn send_pending_packets(
    connections: &ConnectionMap,
    socket: &Rc<QuicUdpSocket>,
    _local_addr: SocketAddr,
    mmsg_batch: usize,
    targets: &[ConnKey],
    dirty_queue: &mut VecDeque<ConnKey>,
) {
    let mut conns = connections.borrow_mut();

    // 送信用スクラッチ（send_buf + 連結バッファ + パケット境界 + 送信エントリ + sendmmsg 配列）を
    // スレッドローカルから払い出して再利用する。thread-per-core のためロック不要。
    // take/replace により .await をまたいでスレッドローカルの borrow を保持しないので、
    // 再入（このループ内での多重呼び出し）でも安全。これにより送信のたびに発生していた
    // malloc を排除する。mmsg バッチ幅は `[http3].mmsg_batch_size`（初回確保時に固定）。
    let mut scratch = take_h3_send_scratch(mmsg_batch);
    let H3SendScratch {
        send_buf,
        batch,
        offsets,
        sends,
        mmsg,
    } = &mut scratch;
    batch.clear();
    offsets.clear();
    sends.clear();
    let gso_enabled = socket.gso_enabled();
    let mut closed = Vec::new();

    // sweep 全体で継続する連結バッファのカーソル。「現在構築中のエントリ」は
    // [cur_batch_start, batch.len()) / offsets[cur_offsets_start..] が表す。
    let mut cur_batch_start = 0usize;
    let mut cur_offsets_start = 0usize;
    let mut seg_size = 0usize;
    let mut cur_dest: Option<SocketAddr> = None;

    for cid in targets {
        // `ConnKey`（`Rc<ConnectionId>`）を `&**cid` で deref し、HashMap キー
        // （素の `ConnectionId<'static>`）としてルックアップする。
        let Some(handler) = conns.get_mut(&**cid) else {
            continue; // 既に削除済み（この 1 イテレーション内では通常起きない）。
        };
        // F-60: GSO セグメントサイズの自動調整。quiche の PMTU 探索結果
        // （`max_send_udp_payload_size`: ハンドシェイク中 1200 → 検証後は
        // 設定上限・経路 MTU の小さい方へ成長）に per-connection で追従し、
        // 下限 MIN_UDP_SEND_PAYLOAD / 上限 send_buf 長でクランプする。
        let max_payload = handler
            .conn
            .max_send_udp_payload_size()
            .clamp(MIN_UDP_SEND_PAYLOAD, send_buf.len());

        // F-151: この接続が 1 パケットでも生成したか（送り残しの可能性があるため、
        // 生成があればダーティのまま残す）。
        let mut sent_any = false;

        loop {
            let (write, send_info) = match handler.conn.send(&mut send_buf[..max_payload]) {
                Ok(v) => {
                    sent_any = true;
                    v
                }
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

            // 現在構築中エントリの状態（sweep 全体で継続する batch/offsets の末尾範囲）。
            let cur_seg_count = offsets.len() - cur_offsets_start;
            let cur_bytes = batch.len() - cur_batch_start;

            // 宛先変更 / セグメントサイズ不一致 / GSO バッチ上限（B-18: EMSGSIZE 回避）で
            // 現在エントリを確定する。**即送信せず** sends へ蓄積する（F-115 第2段）。
            let dest_changed = cur_dest.is_some_and(|d| d != send_info.to);
            if dest_changed
                || gso_batch_must_flush_before_append(cur_seg_count, cur_bytes, write, seg_size)
            {
                finalize_send_entry(
                    sends,
                    batch.len(),
                    offsets.as_slice(),
                    gso_enabled,
                    &mut cur_batch_start,
                    &mut cur_offsets_start,
                    &mut seg_size,
                    &mut cur_dest,
                );
            }

            if offsets.len() == cur_offsets_start {
                // 現在エントリの先頭セグメント。
                seg_size = write;
            }
            let start = batch.len();
            batch.extend_from_slice(&send_buf[..write]);
            offsets.push((start, write));
            cur_dest = Some(send_info.to);

            // GSO セグメント上限 or 最終ショートセグメント（< seg_size）→ エントリ確定。
            if (offsets.len() - cur_offsets_start) >= MAX_GSO_SEGMENTS || write < seg_size {
                finalize_send_entry(
                    sends,
                    batch.len(),
                    offsets.as_slice(),
                    gso_enabled,
                    &mut cur_batch_start,
                    &mut cur_offsets_start,
                    &mut seg_size,
                    &mut cur_dest,
                );
                // ここでは現在エントリが空なので、蓄積が閾値を超えたら sendmmsg で送出して
                // batch/offsets/sends をクリアし継続する（RefCell 借用中の await は既存の
                // 単一借用者モデルに準拠）。
                if sends.len() >= mmsg.batch_size() || batch.len() >= H3_SEND_ACCUM_MAX_BYTES {
                    send_mmsg_flush(socket, batch.as_slice(), sends.as_slice(), mmsg).await;
                    batch.clear();
                    offsets.clear();
                    sends.clear();
                    cur_batch_start = 0;
                    cur_offsets_start = 0;
                }
            }
        }

        // 接続末尾: 構築中エントリを確定（送信は閾値到達時 or sweep 末尾）。
        finalize_send_entry(
            sends,
            batch.len(),
            offsets.as_slice(),
            gso_enabled,
            &mut cur_batch_start,
            &mut cur_offsets_start,
            &mut seg_size,
            &mut cur_dest,
        );
        // 接続境界でも蓄積が閾値を超えていれば途中送出する（現在エントリは確定済みで空）。
        // 多接続（各接続が少数パケット）でも蓄積メモリを ~256KB / batch エントリ以内に抑える。
        if sends.len() >= mmsg.batch_size() || batch.len() >= H3_SEND_ACCUM_MAX_BYTES {
            send_mmsg_flush(socket, batch.as_slice(), sends.as_slice(), mmsg).await;
            batch.clear();
            offsets.clear();
            sends.clear();
            cur_batch_start = 0;
            cur_offsets_start = 0;
        }

        if handler.conn.is_closed() {
            debug!("[HTTP/3] Connection closed from {}", handler.peer_addr);
            closed.push(cid.clone());
        } else if sent_any && !handler.dirty {
            // F-151: 送り残しがあるかもしれないため、パケットを生成した接続はダーティの
            // まま残す。`handler` を既に可変借用中のため `mark_dirty` は使わずインラインで
            // フラグを立てる（`conns` の二重借用を避けるため）。
            handler.dirty = true;
            dirty_queue.push_back(cid.clone());
        }
    }

    // sweep 末尾: 残りをまとめて送出（現在エントリは上のループで確定済み）。
    if !sends.is_empty() {
        send_mmsg_flush(socket, batch.as_slice(), sends.as_slice(), mmsg).await;
    }

    for cid in closed {
        conns.remove(&*cid);
    }

    // スクラッチをスレッドローカルへ返却し、次回呼び出しで再利用する（malloc 排除）。
    put_h3_send_scratch(scratch);
}

/// F-115 第2段: 送信エントリ 1 件（`batch` 内の範囲）を表す。`sends` へ蓄積され、
/// sweep 末尾/閾値到達で sendmmsg によりまとめて送出される。ライフタイムを持たないインデックス
/// 表現なので、`H3SendScratch` に載せてスイープ間で再利用できる（per-sweep 確保なし）。
struct SendRaw {
    /// `batch` 内の開始オフセット。
    batch_start: usize,
    /// 連結長（= GSO バッチ全体 or 単一パケット長）。
    len: usize,
    /// GSO セグメントサイズ。
    seg_size: u16,
    /// パケット数（1 なら sendmmsg 側で UDP_SEGMENT cmsg を付けない）。
    segments: u16,
    /// 送信先。
    dest: SocketAddr,
}

/// 構築中のエントリ（`offsets[cur_offsets_start..]` / `batch[cur_batch_start..batch_len]`）を
/// 確定し `sends` へ積む。**送信はしない**（sendmmsg は呼び出し側でまとめて行う）。
///
/// GSO 無効時は multi-segment エントリをパケット境界ごとの単一パケットエントリへ展開する
/// （cmsg 無しの純 sendmmsg。設計判断: F-115 第2段 §2「GSO 無効フォールバック」）。確定後は
/// カーソル（cur_batch_start / cur_offsets_start）を末尾へ進め、seg_size / cur_dest をリセットする。
fn finalize_send_entry(
    sends: &mut Vec<SendRaw>,
    batch_len: usize,
    offsets: &[(usize, usize)],
    gso_enabled: bool,
    cur_batch_start: &mut usize,
    cur_offsets_start: &mut usize,
    seg_size: &mut usize,
    cur_dest: &mut Option<SocketAddr>,
) {
    let seg_count = offsets.len() - *cur_offsets_start;
    if seg_count == 0 {
        // 構築中エントリが空。カーソルだけ現在位置へ揃えてリセット。
        *cur_batch_start = batch_len;
        *seg_size = 0;
        *cur_dest = None;
        return;
    }
    // セグメントがある以上 cur_dest は Some。念のため None は安全側でリセットして戻る。
    let Some(dest) = *cur_dest else {
        *cur_batch_start = batch_len;
        *cur_offsets_start = offsets.len();
        *seg_size = 0;
        return;
    };

    if gso_enabled || seg_count == 1 {
        // GSO 有効 or 単一パケット: エントリ 1 件（segments>1 は sendmmsg 側で UDP_SEGMENT cmsg）。
        sends.push(SendRaw {
            batch_start: *cur_batch_start,
            len: batch_len - *cur_batch_start,
            seg_size: *seg_size as u16,
            segments: seg_count as u16,
            dest,
        });
    } else {
        // GSO 無効: パケット境界ごとに 1 エントリへ展開（cmsg なしでも per-packet syscall は削減）。
        for &(off, len) in &offsets[*cur_offsets_start..] {
            sends.push(SendRaw {
                batch_start: off,
                len,
                seg_size: len as u16,
                segments: 1,
                dest,
            });
        }
    }

    *cur_batch_start = batch_len;
    *cur_offsets_start = offsets.len();
    *seg_size = 0;
    *cur_dest = None;
}

/// 蓄積した送信エントリ `sends`（`batch` 内の範囲を指す）を sendmmsg でまとめて送出する。
///
/// `scratch.batch_size()` ごとにチャンク分割し、各チャンクをスタック配列（`MMSG_BATCH_MAX` 上限、
/// ヒープ確保なし）で組み立てて `send_mmsg_async` へ渡す。`sends` は GSO 無効展開により
/// バッチ幅を超え得るため、ここでチャンク化して漏れなく送出する。
async fn send_mmsg_flush(
    socket: &Rc<QuicUdpSocket>,
    batch: &[u8],
    sends: &[SendRaw],
    scratch: &mut crate::udp::socket::MmsgSendScratch,
) {
    // F-130 C3: パイプライン化 io_uring SENDMSG が有効なら、libc sendmmsg を使わずに
    // `IORING_OP_SENDMSG` を複数 SQE / 1 submit でまとめて送出する。
    #[cfg(all(target_os = "linux", veil_rt_uring))]
    {
        if let Some(mut send) = take_uring_udp_send() {
            let batch_n = send.capacity();
            let mut i = 0;
            while i < sends.len() {
                let k = (sends.len() - i).min(batch_n);
                // k..MMSG_BATCH_MAX は末尾要素の複製で埋め、`[..k]` で捨てる（複製分は送出されない）。
                let chunk: [crate::udp::socket::SendmmsgEntry; crate::udp::socket::MMSG_BATCH_MAX] =
                    std::array::from_fn(|j| {
                        let s = &sends[i + j.min(k - 1)];
                        crate::udp::socket::SendmmsgEntry {
                            data: &batch[s.batch_start..s.batch_start + s.len],
                            seg_size: s.seg_size,
                            segments: s.segments,
                            dest: s.dest,
                        }
                    });
                if let Err(e) = send.send_batch(&chunk[..k]).await {
                    warn!("[HTTP/3] io_uring sendmsg flush error: {}", e);
                }
                i += k;
            }
            put_uring_udp_send(send);
            return;
        }
    }

    // フォールバック（reactor ビルド / io_uring パイプライン無効時）: 従来の libc sendmmsg。
    let batch_n = scratch.batch_size();
    let mut i = 0;
    while i < sends.len() {
        let k = (sends.len() - i).min(batch_n);
        // k..MMSG_BATCH_MAX は末尾要素の複製で埋め、`[..k]` で捨てる（複製分は送出されない）。
        let chunk: [crate::udp::socket::SendmmsgEntry; crate::udp::socket::MMSG_BATCH_MAX] =
            std::array::from_fn(|j| {
                let s = &sends[i + j.min(k - 1)];
                crate::udp::socket::SendmmsgEntry {
                    data: &batch[s.batch_start..s.batch_start + s.len],
                    seg_size: s.seg_size,
                    segments: s.segments,
                    dest: s.dest,
                }
            });
        if let Err(e) = socket.send_mmsg_async(&chunk[..k], scratch).await {
            warn!("[HTTP/3] sendmmsg flush error: {}", e);
        }
        i += k;
    }
}

/// GSO セグメント上限（UDP GSO の一般的な最大セグメント数）
const MAX_GSO_SEGMENTS: usize = 64;

/// F-115: 1 回の readiness あたり非ブロッキングで掻き出す追加データグラム数の**既定**上限。
/// select/タイマー往復を drain バッチ全体で 1 回に償却しつつ、送信・タイムアウト・notify を
/// 過度に遅延させないための上限（Docker veth では 1 データグラム 1 recvmsg のため、この値まで
/// 連続受信すると 1 回の送信スイープへまとめられる）。
///
/// **F-152: この値は `[http3] recv_drain_max` で設定可能になった**（本定数は既定値）。
/// reactor バックエンド（FreeBSD/OpenBSD/NetBSD/macOS・Linux の `--features epoll`）
/// でのみ参照する。Linux 既定の io_uring 経路は `mmsg_batch_size` 本の
/// `IORING_OP_RECVMSG` パイプライン（F-130）なので本値を使わない。
pub const H3_RECV_DRAIN_MAX_DEFAULT: usize = 64;

/// F-152: `[http3] recv_drain_max` のクランプ上限。
///
/// 1 イテレーションで掻き出すデータグラムを増やすほど固定費は償却されるが、
/// その間は送信・タイムアウト・バックエンド通知が待たされるため上限を設ける。
/// 受信バッファは `mmsg_batch_size` 個ぶんを使い回すだけなので、本値を大きくしても
/// メモリ使用量は増えない（ループ回数が増えるだけ）。
pub const H3_RECV_DRAIN_MAX_LIMIT: usize = 4096;

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

/// F-115 第2段: 1 sweep の sendmmsg 蓄積バッファ（`batch`）の途中送出しきい値。
/// エントリ確定（現在エントリが空）のたびにこの値を超えていれば sendmmsg で送出して
/// batch/offsets/sends をクリアし、蓄積バッファの肥大とレイテンシ増を防ぐ。256KB は
/// 多接続時でも 1 回の sendmmsg（最大 16 メッセージ）に見合う実務的な上限（設計書 §2）。
const H3_SEND_ACCUM_MAX_BYTES: usize = 256 * 1024;

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
    /// GSO バッチ連結バッファ（F-115 第2段: sweep 全体で追記し続ける）
    batch: Vec<u8>,
    /// バッチ内のパケット境界 (offset, len)（sweep 全体で保持）
    offsets: Vec<(usize, usize)>,
    /// F-115 第2段: 確定済み送信エントリ列（`batch` 内範囲のインデックス表現）。
    sends: Vec<SendRaw>,
    /// F-115 第2段: sendmmsg 用スクラッチ（Box 固定配列を再利用）。
    mmsg: crate::udp::socket::MmsgSendScratch,
}

thread_local! {
    /// 送信スクラッチのスレッドローカル保管庫。thread-per-core のためロック不要。
    static H3_SEND_SCRATCH: std::cell::RefCell<Option<H3SendScratch>> =
        const { std::cell::RefCell::new(None) };
}

// F-130 C3: パイプライン化 io_uring SENDMSG ハンドル（Linux uring バックエンドのみ）。
// `run_http3_server_async` が起動時に 1 回だけ設定し、`send_mmsg_flush` が take/put で
// 払い出す（.await をまたいで RefCell borrow を保持しないため H3_SEND_SCRATCH と同方針）。
#[cfg(all(target_os = "linux", veil_rt_uring))]
thread_local! {
    static URING_UDP_SEND: std::cell::RefCell<Option<crate::runtime::udp_send::UringUdpSend>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(all(target_os = "linux", veil_rt_uring))]
fn take_uring_udp_send() -> Option<crate::runtime::udp_send::UringUdpSend> {
    URING_UDP_SEND.with(|c| c.borrow_mut().take())
}

#[cfg(all(target_os = "linux", veil_rt_uring))]
fn put_uring_udp_send(send: crate::runtime::udp_send::UringUdpSend) {
    URING_UDP_SEND.with(|c| *c.borrow_mut() = Some(send));
}

/// スクラッチを払い出す（無ければ新規確保）。take してから返すため、.await をまたいで
/// スレッドローカルの borrow を保持しない（再入安全）。
///
/// `mmsg_batch` は初回確保時のみ効く（既存スクラッチのバッチ幅は維持）。
fn take_h3_send_scratch(mmsg_batch: usize) -> H3SendScratch {
    H3_SEND_SCRATCH
        .with(|s| s.borrow_mut().take())
        .unwrap_or_else(|| H3SendScratch {
            send_buf: vec![0u8; MAX_UDP_SEND_PAYLOAD],
            batch: Vec::new(),
            offsets: Vec::new(),
            sends: Vec::new(),
            mmsg: crate::udp::socket::MmsgSendScratch::with_batch(mmsg_batch),
        })
}

/// スクラッチを返却する（次回再利用）。肥大化した batch は一定上限で解放してメモリを抑える。
fn put_h3_send_scratch(mut scratch: H3SendScratch) {
    scratch.batch.clear();
    scratch.offsets.clear();
    scratch.sends.clear();
    // バッチが極端に肥大化した場合（>1MB）は確保を手放す。
    if scratch.batch.capacity() > (1 << 20) {
        scratch.batch.shrink_to(64 * 1500);
    }
    H3_SEND_SCRATCH.with(|s| *s.borrow_mut() = Some(scratch));
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

/// F-169 Part B: zstd 圧縮コンテキストをスレッドローカルに保持して使い回す。
///
/// `src/proxy.rs` の同名ヘルパーと同じ設計・同じ注意点（詳細はそちらの doc 参照）。
/// **「確保が減った」ことと「速くなった」ことは別の主張**であり（`AGENTS.md` F-168 の
/// 教訓）、本変更はスループット改善を約束するものではない。
#[cfg(feature = "compression")]
fn zstd_compress_reuse_ctx(body: &[u8], level: i32) -> Vec<u8> {
    use std::cell::RefCell;

    thread_local! {
        static ZSTD_COMPRESSOR: RefCell<Option<zstd::bulk::Compressor<'static>>> =
            const { RefCell::new(None) };
    }

    ZSTD_COMPRESSOR.with(|cell| {
        let mut slot = cell.borrow_mut();
        let compressor = match slot.as_mut() {
            Some(c) => c,
            None => {
                let c = match zstd::bulk::Compressor::new(level) {
                    Ok(c) => c,
                    Err(_) => return body.to_vec(),
                };
                slot.get_or_insert(c)
            }
        };
        if compressor.set_compression_level(level).is_err() {
            return body.to_vec();
        }
        compressor.compress(body).unwrap_or_else(|_| body.to_vec())
    })
}

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
        AcceptedEncoding::Zstd => zstd_compress_reuse_ctx(body, compression.zstd_level),
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

    /// F-152: `[http3] recv_drain_max`（reactor 経路の 1 イテレーションあたり
    /// データグラム drain 上限）の既定値とクランプ範囲。
    ///
    /// 既定値は従来ハードコードされていた `H3_RECV_DRAIN_MAX`（= 64）と同値であること
    /// （設定を書かなければ挙動が変わらないことの担保）。
    #[test]
    fn test_recv_drain_max_default_and_clamp() {
        assert_eq!(
            H3_RECV_DRAIN_MAX_DEFAULT, 64,
            "既定値は従来の定数と同値であること"
        );
        assert_eq!(
            Http3ServerConfig::default().recv_drain_max,
            H3_RECV_DRAIN_MAX_DEFAULT
        );

        // メインループが適用するクランプと同じ式で範囲を検証する。
        let clamp = |n: usize| n.clamp(1, H3_RECV_DRAIN_MAX_LIMIT);
        assert_eq!(clamp(0), 1, "0 は 1 へ引き上げる（無限ループ防止）");
        assert_eq!(clamp(1), 1);
        assert_eq!(clamp(64), 64);
        assert_eq!(clamp(1024), 1024, "上限内はそのまま使える");
        assert_eq!(
            clamp(H3_RECV_DRAIN_MAX_LIMIT + 1),
            H3_RECV_DRAIN_MAX_LIMIT,
            "上限超過はクランプする"
        );
        assert_eq!(clamp(usize::MAX), H3_RECV_DRAIN_MAX_LIMIT);
    }

    #[test]
    fn test_config_default() {
        let config = Http3ServerConfig::default();
        assert_eq!(config.recv_drain_max, H3_RECV_DRAIN_MAX_DEFAULT);
        assert_eq!(config.max_idle_timeout, 30000);
        assert_eq!(config.max_udp_payload_size, 1350);
    }

    // ====================
    // F-151: ダーティ接続集合 + タイマー最小ヒープ
    // ====================

    /// F-151: `mark_dirty_flag` が多重登録しないこと、`dirty=false` に戻したあとは
    /// 再登録できることを検証する（quiche/`Http3Handler` に依存しない純粋ヘルパー）。
    #[test]
    fn test_mark_dirty_flag_dedup_and_reregister() {
        let mut dirty = false;
        let mut queue: VecDeque<u32> = VecDeque::new();

        // 初回登録: push される。
        assert!(mark_dirty_flag(&mut dirty, &mut queue, &1));
        assert!(dirty);
        assert_eq!(queue.len(), 1);

        // 既にダーティ: 多重登録されない。
        assert!(!mark_dirty_flag(&mut dirty, &mut queue, &1));
        assert!(dirty);
        assert_eq!(queue.len(), 1, "多重登録されないこと");

        // dirty=false に戻したあとは再登録できる。
        dirty = false;
        queue.pop_front();
        assert!(mark_dirty_flag(&mut dirty, &mut queue, &1));
        assert!(dirty);
        assert_eq!(queue.len(), 1);
    }

    /// F-151: タイマーヒープの遅延削除。同じ cid で期限を 2 回 push したあと、
    /// 古い方の期限が来ても「現在の `timer_deadline` と一致しない」ため無視され、
    /// 新しい方でのみ発火することを検証する。
    #[test]
    fn test_timer_entry_delayed_deletion() {
        let base = Instant::now();
        let old_deadline = base + Duration::from_millis(10);
        let new_deadline = base + Duration::from_millis(50);

        // 期限が更新される前に登録された古いエントリが pop されたとき、
        // 現在の期限（既に new_deadline に更新済み）とは一致しないので無視される。
        assert!(
            !timer_entry_is_current(old_deadline, Some(new_deadline)),
            "古い期限は無視されること"
        );

        // 新しい期限がそのまま pop されれば、現在の期限と一致するので発火する。
        assert!(
            timer_entry_is_current(new_deadline, Some(new_deadline)),
            "最新の期限は発火すること"
        );

        // 接続が既に削除済み（timer_deadline が None）なら常に無視される。
        assert!(!timer_entry_is_current(new_deadline, None));

        // 実際の BinaryHeap<Reverse<..>> でも最小（最も早い）期限から pop されることを
        // あわせて確認する（遅延削除の前提となる pop 順序）。
        let mut heap: BinaryHeap<Reverse<(Instant, u32)>> = BinaryHeap::new();
        heap.push(Reverse((old_deadline, 1)));
        heap.push(Reverse((new_deadline, 1)));
        let Reverse((first_popped, _)) = heap.pop().unwrap();
        assert_eq!(first_popped, old_deadline, "古い期限が先に pop されること");
        assert!(!timer_entry_is_current(first_popped, Some(new_deadline)));
        let Reverse((second_popped, _)) = heap.pop().unwrap();
        assert_eq!(second_popped, new_deadline);
        assert!(timer_entry_is_current(second_popped, Some(new_deadline)));
    }

    /// F-151: `select` の sleep 時間算出。ヒープ空 → 100ms、先頭が未来 → その差分、
    /// 先頭が過去 → 0、上限 100ms クランプ。
    #[test]
    fn test_next_sleep_duration() {
        let now = Instant::now();

        // ヒープが空 → 既定 100ms。
        assert_eq!(
            next_sleep_duration(None, now, false),
            Duration::from_millis(100)
        );

        // 先頭が未来（30ms 後）→ その差分。
        let future = now + Duration::from_millis(30);
        assert_eq!(
            next_sleep_duration(Some(future), now, false),
            Duration::from_millis(30)
        );

        // 先頭が過去 → 0。
        let past = now - Duration::from_millis(5);
        assert_eq!(next_sleep_duration(Some(past), now, false), Duration::ZERO);

        // 先頭が過去と現在同時刻（境界）→ 0。
        assert_eq!(next_sleep_duration(Some(now), now, false), Duration::ZERO);

        // 先頭が遠い未来（500ms 後）→ 100ms にクランプ。
        let far_future = now + Duration::from_millis(500);
        assert_eq!(
            next_sleep_duration(Some(far_future), now, false),
            Duration::from_millis(100)
        );

        // レビュー修正: ダーティ接続が残っている（has_dirty = true）なら、
        // タイマーヒープの先頭がどれだけ未来でも常に 0（sleep しない）。
        // これをしないとストリーミング中のレスポンスがチャンクごとに最大 100ms
        // 停止し得るため（致命的なレイテンシ退行）。
        assert_eq!(next_sleep_duration(None, now, true), Duration::ZERO);
        assert_eq!(
            next_sleep_duration(Some(far_future), now, true),
            Duration::ZERO
        );
        assert_eq!(next_sleep_duration(Some(past), now, true), Duration::ZERO);
    }

    /// B-43: PartialResponse の状態表現の不変条件を検証する。
    ///
    /// 実際の再送（`try_flush_partial`）は生きた h3::Connection（QUIC ハンドシェイク
    /// 完了）が必要なため単体では駆動できず、E2E/負荷再現は別途行う。ここでは
    /// ヘルパーが依存する状態表現（ヘッダのみ応答 vs ボディ応答、残バイトのスライス計算、
    /// 完了判定）が正しいことを検証する。
    #[test]
    fn test_b43_partial_response_state() {
        // ヘッダのみ応答（リダイレクト等）: head=Some, body 空。
        // try_flush_partial はヘッダ送出成功時に fin=true（body 空）で即完了する。
        let redirect = PartialResponse {
            head: Some(vec![h3::Header::new(b":status", b"302")]),
            body: Vec::new(),
            written: 0,
        };
        assert!(redirect.head.is_some(), "ヘッダ未送出であること");
        assert!(redirect.body.is_empty(), "ボディ無し = fin=true で完了扱い");

        // ボディ応答: StreamBlocked で保存された初期状態。
        let mut pr = PartialResponse {
            head: Some(vec![h3::Header::new(b":status", b"200")]),
            body: b"hello world".to_vec(),
            written: 0,
        };

        // ヘッダ送出成功を模擬（try_flush_partial の Ok アーム相当）。
        pr.head = None;
        assert!(!pr.body.is_empty(), "ボディありなのでボディ送出フェーズへ");

        // 部分送出を模擬し、残バイトのスライス計算と完了判定を検証。
        let first = 5usize;
        assert_eq!(&pr.body[pr.written..], b"hello world");
        pr.written += first;
        assert_eq!(
            &pr.body[pr.written..],
            b" world",
            "残バイトのみ再送されること"
        );
        assert!(pr.written < pr.body.len(), "まだ未完了");

        pr.written += pr.body.len() - pr.written;
        assert!(pr.written >= pr.body.len(), "全量送出で完了判定");
    }

    /// F-101: ヘッダブロックサイズ合計が MAX_HEADER_SIZE 判定に使えること
    #[test]
    fn test_h3_request_header_block_size() {
        let small = vec![
            h3::Header::new(b":method", b"GET"),
            h3::Header::new(b":path", b"/"),
            h3::Header::new(b":authority", b"localhost"),
            h3::Header::new(b":scheme", b"https"),
        ];
        let small_sz = h3_request_header_block_size(&small);
        assert!(small_sz < MAX_HEADER_SIZE);
        assert_eq!(
            small_sz,
            b":method".len()
                + b"GET".len()
                + b":path".len()
                + b"/".len()
                + b":authority".len()
                + b"localhost".len()
                + b":scheme".len()
                + b"https".len()
        );

        let big_val = vec![b'A'; 9000];
        let big = vec![
            h3::Header::new(b":method", b"GET"),
            h3::Header::new(b":path", b"/"),
            h3::Header::new(b":authority", b"localhost"),
            h3::Header::new(b":scheme", b"https"),
            h3::Header::new(b"x-huge", &big_val),
        ];
        assert!(h3_request_header_block_size(&big) > MAX_HEADER_SIZE);
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

    // --------------------
    // B-38 / B-39 単体テスト
    // --------------------

    /// B-39: gRPC はルート prefix を剥がさずフルパスを維持する
    #[test]
    fn test_b39_upstream_path_preserves_grpc_full_path() {
        let prefix = b"/grpc.test.v1.TestService";
        let path = "/grpc.test.v1.TestService/UnaryCall";
        let got = compute_upstream_request_path(path, prefix, "", true);
        assert_eq!(got, path, "gRPC must keep full service/method path");
    }

    /// B-39: 非 gRPC は /* プレフィックスを除去する
    #[test]
    fn test_b39_upstream_path_strips_wildcard_prefix() {
        let prefix = b"/api";
        let path = "/api/v1/items";
        let got = compute_upstream_request_path(path, prefix, "", false);
        assert_eq!(got, "/v1/items");

        // target path_prefix 前置
        let got2 = compute_upstream_request_path(path, prefix, "/backend", false);
        assert_eq!(got2, "/backend/v1/items");

        // prefix なし
        assert_eq!(
            compute_upstream_request_path("/health", b"", "", false),
            "/health"
        );

        // 空パスは /
        assert_eq!(compute_upstream_request_path("", b"", "", true), "/");
        assert_eq!(compute_upstream_request_path("", b"", "", false), "/");
    }

    /// B-39: compute_backend_path は preserve_full=false と同等
    #[test]
    fn test_compute_backend_path_matches_upstream_helper() {
        let target = ProxyTarget::parse("http://127.0.0.1:9004").expect("target");
        let path = b"/grpc.test.v1.TestService/UnaryCall";
        let prefix = b"/grpc.test.v1.TestService";
        let a = compute_backend_path(&target, path, prefix);
        let b = compute_upstream_request_path(
            std::str::from_utf8(path).unwrap(),
            prefix,
            &target.path_prefix,
            false,
        );
        assert_eq!(a, b);
        // プレフィックス除去後
        assert_eq!(a, "/UnaryCall");
    }

    /// B-39: trailers をヘッダへマージし、重複名は既存を優先
    #[test]
    fn test_b39_merge_response_headers_and_trailers() {
        let headers = vec![
            (b"content-type".to_vec(), b"application/grpc".to_vec()),
            (b"connection".to_vec(), b"close".to_vec()),
            (b"content-length".to_vec(), b"0".to_vec()),
        ];
        let trailers = vec![
            (b"grpc-status".to_vec(), b"0".to_vec()),
            (b"grpc-message".to_vec(), b"ok".to_vec()),
            // 既存 content-type は上書きしない
            (b"content-type".to_vec(), b"should-not-win".to_vec()),
        ];

        let merged = merge_response_headers_and_trailers(&headers, &trailers, false);
        assert!(!merged
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case(b"connection")));
        assert!(merged
            .iter()
            .any(|(n, v)| n.eq_ignore_ascii_case(b"grpc-status") && v == b"0"));
        assert!(merged
            .iter()
            .any(|(n, v)| n.eq_ignore_ascii_case(b"content-type") && v == b"application/grpc"));

        // 圧縮時は content-length / content-encoding を落とす
        let compressed = merge_response_headers_and_trailers(&headers, &[], true);
        assert!(!compressed
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case(b"content-length")));
    }

    /// parse_http_response: 正常系と trailers 空
    #[test]
    fn test_parse_http_response_basic() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello";
        let r = parse_http_response(raw).expect("parse");
        assert_eq!(r.status_code, 200);
        assert_eq!(r.body, b"hello");
        assert!(r.trailers.is_empty());
        assert!(r
            .headers
            .iter()
            .any(|(n, v)| n.eq_ignore_ascii_case(b"content-type") && v == b"text/plain"));
    }

    /// parse_http_response: 不正レスポンスは Err
    #[test]
    fn test_parse_http_response_invalid() {
        let raw = b"not-http-at-all";
        assert!(parse_http_response(raw).is_err());
    }

    /// find_header_end / parse_status_code
    #[test]
    fn test_parse_status_and_header_end() {
        assert_eq!(find_header_end(b"HTTP/1.1 404 N\r\n\r\n"), Some(14));
        assert_eq!(
            parse_status_code(b"HTTP/1.1 502 Bad Gateway\r\n"),
            Some(502)
        );
        assert_eq!(parse_status_code(b"garbage"), None);
    }

    /// B-38: モジュール空ならヘッダをそのまま返す（WASM エンジン不要）
    #[cfg(feature = "wasm")]
    #[test]
    fn test_b38_apply_h3_wasm_empty_modules_passthrough() {
        let modules = std::sync::Arc::new(Vec::new());
        let headers = vec![(b"x-a".to_vec(), b"1".to_vec())];
        let out = futures::executor::block_on(apply_h3_wasm_response_headers(
            &modules,
            200,
            headers.clone(),
        ));
        assert_eq!(out, headers);
    }

    /// B-41: 初期ヘッダから grpc-status/message と CL を除外し trailers 専用にする
    #[cfg(feature = "grpc")]
    #[test]
    fn test_b41_filter_h3_grpc_initial_headers() {
        let headers: &[(&[u8], &[u8])] = &[
            (b":status", b"200"),
            (b"content-type", b"application/grpc"),
            (b"grpc-status", b"0"),
            (b"grpc-message", b"ok"),
            (b"content-length", b"12"),
            (b"x-server-id", b"grpc-server"),
            (b"date", b"Fri, 10 Jul 2026 00:00:00 GMT"),
        ];
        let kept: Vec<(&[u8], &[u8])> = filter_h3_grpc_initial_headers(headers).collect();
        assert_eq!(kept.len(), 2);
        assert!(kept
            .iter()
            .any(|(n, v)| *n == b"x-server-id" && *v == b"grpc-server"));
        assert!(kept.iter().any(|(n, _)| *n == b"date"));
        assert!(!kept
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case(b"grpc-status")));
        assert!(!kept
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case(b"content-length")));
    }

    /// B-39: content-type から gRPC を検出
    #[cfg(feature = "grpc")]
    #[test]
    fn test_b39_header_pairs_indicate_grpc() {
        assert!(header_pairs_indicate_grpc(&[(
            b"content-type".to_vec(),
            b"application/grpc".to_vec()
        )]));
        assert!(header_pairs_indicate_grpc(&[(
            b"Content-Type".to_vec(),
            b"application/grpc+proto".to_vec()
        )]));
        assert!(!header_pairs_indicate_grpc(&[(
            b"content-type".to_vec(),
            b"application/json".to_vec()
        )]));
        assert!(!header_pairs_indicate_grpc(&[]));
    }

    /// Http3Handler::is_grpc_request と同等の content-type 判定
    #[cfg(feature = "grpc")]
    #[test]
    fn test_b39_is_grpc_content_type_via_headers_module() {
        assert!(crate::grpc::headers::is_grpc_content_type(
            b"application/grpc"
        ));
        assert!(crate::grpc::headers::is_grpc_content_type(
            b"application/grpc+proto"
        ));
        assert!(!crate::grpc::headers::is_grpc_content_type(b"text/plain"));
    }
}
