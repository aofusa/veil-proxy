//! # HTTP/3 サーバー（独自ランタイム + quiche）
//!
//! veil の独自ランタイム（`src/runtime/`。Linux 既定は io_uring、`epoll`・BSD・macOS・Windows は
//! readiness reactor）と Cloudflare quiche を使用した HTTP/3 サーバー実装。thread-per-core モデルで、
//! ワーカーごとに 1 本のメインループが自分の UDP ソケットの全接続を処理する。
//!
//! ## 設計ポイント
//!
//! - **UDP I/O**: io_uring は `recvmsg` のバッチ（`[http3] mmsg_batch_size`）、reactor は
//!   `recv_drain_max` まで読み切る。送信は GSO（F-111/F-115）
//! - **イベント駆動**: 仕事のある接続だけをダーティ集合で回す（全接続スイープ禁止。B-12/F-151）
//! - **タイマー管理**: quiche::timeout() と `runtime::time` の連携
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
use crate::tls_provider::{hmac, SecureRandom, SystemRandom};
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

// F-233: 機能ごとのファイル（子モジュールは `use super::*;` で親の名前を見る）
mod request_task;
use self::request_task::*;
mod streaming;
use self::streaming::*;
mod upstream;
pub use self::upstream::*;
mod event_loop;
pub use self::event_loop::*;

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

/// HTTP/3 のフィールドセクション（HEADERS フレーム）の検査結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum H3FieldSection {
    /// 疑似ヘッダを持ち、リクエストの必須要件を満たす（新規リクエスト）。
    Request,
    /// 疑似ヘッダを 1 つも持たない（既存ストリームなら trailers、新規ストリームなら malformed）。
    NoPseudo,
}

/// HTTP/3 の H3_MESSAGE_ERROR（RFC 9114 §8.1）。
const H3_MESSAGE_ERROR: u64 = 0x10e;
/// HTTP/3 の H3_NO_ERROR（RFC 9114 §8.1。graceful shutdown の CONNECTION_CLOSE。F-187）。
const H3_NO_ERROR_CODE: u64 = 0x100;
/// HTTP/3 の H3_REQUEST_REJECTED（RFC 9114 §8.1。GOAWAY 後に届いた要求。クライアントは再送してよい）。
const H3_REQUEST_REJECTED: u64 = 0x10b;
/// HTTP/3 の H3_EXCESSIVE_LOAD（RFC 9114 §8.1）。バッファ経路の要求本文が上限を超えたストリームを断る。
const H3_EXCESSIVE_LOAD: u64 = 0x107;

/// RFC 9114 §4.1.2 / §4.2 / §4.3.1 に従ってフィールドセクションを検査する。
///
/// `Err` は malformed（呼び出し側が H3_MESSAGE_ERROR で扱う）。検査するのは
/// 疑似ヘッダの重複・未知/禁止（`:status`、拡張 CONNECT を広告していないので `:protocol` も）・
/// 通常フィールドの後ろの疑似ヘッダ・大文字のフィールド名・接続固有フィールド・
/// リクエストの必須疑似ヘッダ（CONNECT とそれ以外で異なる）。
///
/// quiche の h3 層はこれらを検査しないため、無ければ `:method` 欠落を GET と見なす等で
/// malformed なリクエストに 200 を返していた（h3spec で検出）。
///
/// ホットパス（リクエストごと）: 確保なし・フィールド列を 1 回走査するだけ。
fn h3_check_field_section<'a, I>(fields: I) -> Result<H3FieldSection, &'static str>
where
    I: IntoIterator<Item = (&'a [u8], &'a [u8])>,
{
    const METHOD: u8 = 1;
    const SCHEME: u8 = 2;
    const AUTHORITY: u8 = 4;
    const PATH: u8 = 8;
    let mut seen: u8 = 0;
    let mut regular_seen = false;
    let mut is_connect = false;
    let mut host_seen = false;
    let mut scheme_needs_authority = false;
    for (name, value) in fields {
        // F-236（F2）: 値に CR・LF・NUL・前後の空白があれば malformed（RFC 9114 §4.2）
        if !crate::http_utils::is_valid_h2_field_value(value) {
            return Err("invalid field value");
        }
        if let Some(pseudo) = name.strip_prefix(b":") {
            if regular_seen {
                return Err("pseudo-header after regular field");
            }
            let bit = match pseudo {
                b"method" => METHOD,
                b"scheme" => SCHEME,
                b"authority" => AUTHORITY,
                b"path" => PATH,
                _ => return Err("unknown or prohibited pseudo-header"),
            };
            if seen & bit != 0 {
                return Err("duplicated pseudo-header");
            }
            seen |= bit;
            if bit == METHOD {
                if !crate::http_utils::is_valid_method(value) {
                    return Err("invalid :method");
                }
                is_connect = value == b"CONNECT";
            } else if bit == SCHEME {
                scheme_needs_authority = value == b"https" || value == b"http";
            } else if bit == PATH && value.is_empty() {
                return Err("empty :path");
            } else if bit == PATH
                && (!crate::http_utils::is_valid_request_target(value)
                    || (value[0] != b'/' && value != b"*"))
            {
                // origin 形式か `*` だけ（RFC 9114 §4.3.1。絶対形式はルートの照合を素通りする）
                return Err("invalid :path");
            }
        } else {
            regular_seen = true;
            if name.iter().any(u8::is_ascii_uppercase) {
                return Err("uppercase field name");
            }
            if !crate::http_utils::is_valid_header_name(name) {
                return Err("invalid field name");
            }
            match name {
                b"connection" | b"keep-alive" | b"proxy-connection" | b"transfer-encoding"
                | b"upgrade" => return Err("connection-specific field"),
                b"te" if value != b"trailers" => return Err("te other than trailers"),
                b"host" => host_seen = true,
                _ => {}
            }
        }
    }
    if seen == 0 {
        return Ok(H3FieldSection::NoPseudo);
    }
    if seen & METHOD == 0 {
        return Err("missing :method");
    }
    if is_connect {
        // §4.4: CONNECT は :scheme と :path を省き、:authority を必須とする。
        if seen & (SCHEME | PATH) != 0 {
            return Err("CONNECT with :scheme or :path");
        }
        if seen & AUTHORITY == 0 {
            return Err("CONNECT without :authority");
        }
    } else if seen & (SCHEME | PATH) != (SCHEME | PATH) {
        return Err("missing :scheme or :path");
    } else if scheme_needs_authority && seen & AUTHORITY == 0 && !host_seen {
        // §4.3.1: http/https は authority 成分が必須 → :authority か Host のどちらかが要る。
        return Err("missing :authority and host");
    }
    Ok(H3FieldSection::Request)
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
/// 5. `/proc/self/fd/<fd>` パスを生成
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
    // 経路 MTU の探索: 1200 から始め、探索のパケットが届いた大きさまで上げる（UDP のソケットは DF。F-240）
    quic_config.discover_pmtu(config.discover_pmtu);
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
            "[HTTP/3] unknown cc_algorithm '{}': {}; falling back to cubic",
            cc_name, e
        );
        let _ = quic_config.set_cc_algorithm_name("cubic");
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
    /// 最大 UDP ペイロードサイズ（経路 MTU の探索の上限）
    pub max_udp_payload_size: u64,
    /// 経路 MTU の探索（quiche `discover_pmtu`）
    pub discover_pmtu: bool,
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
    /// QUIC 輻輳制御アルゴリズム名（quiche `set_cc_algorithm_name`）。デフォルト: `"cubic"`（v0.8.0）
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
    /// （`[http3] recv_drain_max`、1..=`H3_RECV_DRAIN_MAX_LIMIT`）。デフォルト: 1024（F-152、v0.8.0 で 64 から変更）
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
            max_udp_payload_size: 1452,
            discover_pmtu: true,
            initial_max_data: 10_000_000,
            initial_max_stream_data_bidi_local: 1_000_000,
            initial_max_stream_data_bidi_remote: 1_000_000,
            initial_max_stream_data_uni: 1_000_000,
            initial_max_streams_bidi: 100,
            initial_max_streams_uni: 100,
            gso_gro_enabled: false,
            cc_algorithm: "cubic".to_string(),
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
    /// フロー制御で部分送信になったボディ断片（`(buf, 送信済みオフセット, fin)`）。
    /// `fin` は最後の断片に終端を載せたもの（送り切ったら応答完了）。
    body_pending: Option<(Bytes, usize, bool)>,
    /// レスポンス終端（EOF）を受信し、fin 送出が必要（フロー制御で保留中）。
    need_fin: bool,
    /// レスポンス fin 送出済み（= レスポンス完了）。
    resp_fin_sent: bool,
    /// StreamBlocked で送出保留中の trailers（gRPC。B-97）。
    #[cfg(any(feature = "grpc", feature = "handler"))]
    trailers_pending: Option<crate::http3_stream::RespHeaders>,

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
            #[cfg(any(feature = "grpc", feature = "handler"))]
            trailers_pending: None,
            req_tx: Some(req_tx),
            req_pending: VecDeque::new(),
            req_readable: false,
            req_eof_seen: !has_body,
            req_bytes_total: 0,
            max_request_body,
            req_too_large: false,
        }
    }

    /// B-97: バッファ経路の要求（本文は受信済み）。要求方向は最初から閉じている。
    fn new_buffered(resp_rx: crate::http3_stream::Receiver<crate::http3_stream::RespMsg>) -> Self {
        Self {
            resp_rx,
            resp_started: false,
            head_pending: None,
            body_pending: None,
            need_fin: false,
            resp_fin_sent: false,
            #[cfg(any(feature = "grpc", feature = "handler"))]
            trailers_pending: None,
            req_tx: None,
            req_pending: VecDeque::new(),
            req_readable: false,
            req_eof_seen: true,
            req_bytes_total: 0,
            max_request_body: 0,
            req_too_large: false,
        }
    }
}

/// バッファリング（非ストリーミング）経路の保留リクエスト（F-32）。
///
/// ストリーミング非適格なリクエストは END_STREAM 受信まで `stream_bodies` にボディを
/// 蓄積し、完了時にバッファ経路の要求タスク（B-97: `H3BufferedTask`）を起動する。
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
    /// 要求本文をストリーミングで受けるネイティブハンドラ（gRPC の双方向等。F-209 #5）→ ハンドラのタスクを spawn。
    #[cfg(feature = "handler")]
    StreamHandler(Box<H3HandlerStream>),
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

/// 応答の本文を quiche の送信バッファへコピーせずに渡すためのバッファの作り方（v0.8.0）。
///
/// quiche の既定（`DefaultBufFactory`）は `send_body` のたびに本文を新しい確保へコピーする。本文は
/// 静的配信のキャッシュや上流の受信バッファの `Bytes` なので、参照カウントのまま渡す
/// （`h3::Connection::send_body_zc`）。quiche は ACK まで保持して再送にも使う。スライスで渡される
/// 経路（ヘッダのフレーム・エラー応答の短い本文）は `buf_from_slice` で 1 回コピーする（既定と同じ）。
#[derive(Clone, Debug, Default)]
pub(crate) struct ZcFactory;

/// [`ZcFactory`] のバッファ（`Bytes` の薄い包み）。
#[derive(Clone, Debug)]
pub(crate) struct ZcBuf(Bytes);

impl AsRef<[u8]> for ZcBuf {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl quiche::BufFactory for ZcFactory {
    type Buf = ZcBuf;

    #[inline]
    fn buf_from_slice(buf: &[u8]) -> ZcBuf {
        ZcBuf(Bytes::copy_from_slice(buf))
    }
}

impl quiche::BufSplit for ZcBuf {
    /// 先頭 `at` バイトを残し、残りを返す（`Bytes::split_off` は参照カウントだけでコピーしない）。
    #[inline]
    fn split_at(&mut self, at: usize) -> Self {
        ZcBuf(self.0.split_off(at))
    }
}

/// HTTP/3 の QUIC 接続（本文をコピーせずに送る [`ZcFactory`] 版）。
pub(crate) type H3Conn = quiche::Connection<ZcFactory>;

type StreamIdHasherBuilder = std::hash::BuildHasherDefault<StreamIdHasher>;
type StreamMap<V> = HashMap<u64, V, StreamIdHasherBuilder>;

/// HTTP/3 コネクションハンドラー
///
/// `quiche::Connection`（[`H3Conn`]）と h3::Connection をセットで保持し、
/// コネクションの寿命の間、同一のインスタンスを維持します。
///
/// HTTP/1.1と同等のルーティング・セキュリティ・プロキシ機能をサポート。
struct Http3Handler {
    /// QUIC コネクション
    conn: H3Conn,
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
    /// B-97: 起動待ちのバッファ経路の要求。メインループがワーカーの `FuturesUnordered` へ
    /// 移して即座に 1 回 poll する（`process_h3_events` の後、`drive_proxy_streams` の前）。
    /// 容量を保持したまま使い回す（要求ごとの確保なし）。
    new_buffered: Vec<(H3BufferedTask, Vec<h3::Header>, Bytes)>,
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
    /// F-187: 受け付けた要求ストリーム ID の最大値 + 4（GOAWAY で「ここまでは処理する」と伝える値）。
    next_req_stream: u64,
    /// F-187: 送った GOAWAY の ID。これ以上の ID の新しい要求は H3_REQUEST_REJECTED で断る。
    goaway_id: Option<u64>,
    /// F-187: graceful shutdown で CONNECTION_CLOSE を出した。
    shutdown_closed: bool,
}

impl Http3Handler {
    /// 新しいハンドラーを作成
    fn new(
        conn: H3Conn,
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
            new_buffered: Vec::new(),
            _conn_metric: Http3ActiveConnGuard::new(),
            metric_open_streams: HashSet::default(),
            dirty: false,
            timer_deadline: None,
            key,
            ev_new_headers: Vec::new(),
            ev_finished: Vec::new(),
            ev_reset: Vec::new(),
            next_req_stream: 0,
            goaway_id: None,
            shutdown_closed: false,
        }
    }

    /// F-187: 処理中の要求が無いか（応答の送出途中・バックエンド待ち・本文の受信途中のいずれも無い）。
    fn has_no_requests(&self) -> bool {
        self.partial_responses.is_empty()
            && self.proxy_streams.is_empty()
            && self.buffered_reqs.is_empty()
            && self.stream_bodies.is_empty()
            && self.new_buffered.is_empty()
    }

    /// F-187: graceful shutdown の 1 歩。最初の呼び出しで GOAWAY を送り（以後の新しい要求は断る）、
    /// 処理中の要求が無くなった時点で CONNECTION_CLOSE（H3_NO_ERROR）を出す。送るものができたら
    /// `true`（呼び出し側がダーティにして送出させる）。停止中のメインループだけが呼ぶ。
    fn shutdown_step(&mut self) -> bool {
        let mut acted = false;
        if self.goaway_id.is_none() {
            if let Some(h3) = self.h3_conn.as_mut() {
                let id = self.next_req_stream;
                if h3.send_goaway(&mut self.conn, id).is_ok() {
                    self.goaway_id = Some(id);
                    acted = true;
                }
            }
        }
        if !self.shutdown_closed && self.has_no_requests() {
            self.shutdown_closed = true;
            let _ = self.conn.close(true, H3_NO_ERROR_CODE, b"");
            acted = true;
        }
        acted
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
    fn process_h3_events(&mut self) -> io::Result<bool> {
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
                        let section =
                            h3_check_field_section(list.iter().map(|h| (h.name(), h.value())));
                        // 既にリクエストを受け付けたストリームの 2 つ目のセクションは trailers。
                        let known_stream = self.proxy_streams.contains_key(&stream_id)
                            || self.buffered_reqs.contains_key(&stream_id)
                            || new_headers.iter().any(|(id, _, _)| *id == stream_id);
                        match section {
                            Ok(H3FieldSection::Request) if !known_stream => {
                                if self.goaway_id.is_some_and(|g| stream_id >= g) {
                                    // F-187: GOAWAY の後に始まった要求は処理しない（再送してよい）。
                                    let _ = self.conn.stream_shutdown(
                                        stream_id,
                                        quiche::Shutdown::Read,
                                        H3_REQUEST_REJECTED,
                                    );
                                    let _ = self.conn.stream_shutdown(
                                        stream_id,
                                        quiche::Shutdown::Write,
                                        H3_REQUEST_REJECTED,
                                    );
                                } else {
                                    self.next_req_stream = self.next_req_stream.max(stream_id + 4);
                                    // :path を正規化する（ルートの照合・検査・上流への要求に同じパスを使う。
                                    // F-236 の再走査の F7）。要るときだけ作り直す
                                    let mut list = list;
                                    if let Some(i) = list.iter().position(|h| h.name() == b":path")
                                    {
                                        if let Some(n) = crate::path_norm::normalize_request_path(
                                            list[i].value(),
                                        ) {
                                            list[i] = quiche::h3::Header::new(b":path", &n);
                                        }
                                    }
                                    new_headers.push((stream_id, list, more_frames));
                                }
                            }
                            // trailers: 転送しない（従来は新規リクエストとして再分類していた）。
                            Ok(H3FieldSection::NoPseudo) if known_stream => {}
                            other => {
                                let reason = match other {
                                    Err(r) => r,
                                    Ok(H3FieldSection::Request) => "pseudo-headers in trailers",
                                    Ok(H3FieldSection::NoPseudo) => "missing :method",
                                };
                                // RFC 9114 §4.1.2: malformed はストリームエラー
                                // H3_MESSAGE_ERROR。§8 によりコネクションエラーとして扱ってよく、
                                // 不正なクライアントとの接続を残す理由が無いため接続ごと閉じる。
                                warn!(
                                    "[HTTP/3] malformed request on stream {} from {}: {}",
                                    stream_id, self.peer_addr, reason
                                );
                                let _ = self.conn.close(true, H3_MESSAGE_ERROR, reason.as_bytes());
                                break;
                            }
                        }
                    }
                    Ok((stream_id, h3::Event::Data)) => {
                        did_work = true;
                        if let Some(ps) = self.proxy_streams.get_mut(&stream_id) {
                            // ストリーミング中: バックプレッシャ対応の pump に委譲。
                            ps.req_readable = true;
                        } else {
                            // バッファ経路（または未分類の初回バッチ）: stream_bodies へ排出。
                            let body = self.stream_bodies.entry(stream_id).or_default();
                            let limit = crate::l7::max_buffered_request_body();
                            loop {
                                // どのルートでも許されない大きさになったら、読むのをやめてストリームを断る
                                // （読み続けると quiche がクレジットを返し続け、無限に溜まる。F-236 の再走査の F4）
                                if body.len() > limit {
                                    let _ = self.conn.stream_shutdown(
                                        stream_id,
                                        quiche::Shutdown::Read,
                                        H3_EXCESSIVE_LOAD,
                                    );
                                    let _ = self.conn.stream_shutdown(
                                        stream_id,
                                        quiche::Shutdown::Write,
                                        H3_EXCESSIVE_LOAD,
                                    );
                                    reset.push(stream_id);
                                    break;
                                }
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
                #[cfg(feature = "handler")]
                Decision::StreamHandler(h) => {
                    let (req_tx, req_rx) = crate::http3_stream::channel::<Bytes>(REQ_CHAN_CAP);
                    let (resp_tx, resp_rx) =
                        crate::http3_stream::channel::<crate::http3_stream::RespMsg>(RESP_CHAN_CAP);
                    let mut ps = ProxyStream::new(resp_rx, req_tx, more_frames, h.max_request_body);
                    if let Some(body) = self.stream_bodies.remove(&stream_id) {
                        if !body.is_empty() {
                            ps.req_bytes_total += body.len() as u64;
                            ps.req_pending.push_back(body.freeze());
                        }
                    }
                    crate::runtime::spawn(h3_api_stream_task(
                        h,
                        req_rx,
                        resp_tx,
                        self.notify.clone(),
                    ));
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
                .map(BytesMut::freeze)
                .unwrap_or_default();
            // B-97: メインループでは await せず、要求 Future をメインループへ渡す（`new_buffered`）。
            // 応答はストリーミング経路と同じ `ProxyStream` で送出し、完了時に
            // `drive_proxy_streams` がメトリクスを閉じる。
            let (resp_tx, resp_rx) =
                crate::http3_stream::channel::<crate::http3_stream::RespMsg>(RESP_CHAN_CAP);
            self.proxy_streams
                .insert(stream_id, ProxyStream::new_buffered(resp_rx));
            self.new_buffered.push((
                H3BufferedTask {
                    stream_id,
                    peer_addr: self.peer_addr,
                    client_ip: self.client_ip,
                    resp_tx: Some(resp_tx),
                    notify: self.notify.clone(),
                },
                br.headers,
                body,
            ));
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

        let backend_result = crate::l7::resolve_route(
            authority,
            path_wo_query,
            method,
            &headers_raw,
            raw_query,
            &self.peer_addr,
            false,
        );

        let (prefix, backend, _route_compression) = match backend_result {
            Some(b) => b,
            None => return Decision::Buffer, // handle_request -> 404（gRPC 含む）
        };

        // F-209 #5: 要求本文をストリーミングで受けるハンドラ（gRPC のルート等）は、本文を待たずに呼んで
        // 届いた順に流す（HTTP/2 と同じ全二重）。本文の無い要求・それ以外のハンドラはバッファ経路。
        #[cfg(feature = "handler")]
        if let Backend::Handler(entry, _, _) = &backend {
            if !entry.streaming_request || !more_frames {
                return Decision::Buffer;
            }
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
            let mut hdrs: Vec<(Bytes, Bytes)> = Vec::with_capacity(headers.len() + 1);
            let mut has_host = false;
            for h in headers.iter().filter(|h| !h.name().starts_with(b":")) {
                has_host |= h.name().eq_ignore_ascii_case(b"host");
                hdrs.push((
                    Bytes::copy_from_slice(h.name()),
                    Bytes::copy_from_slice(h.value()),
                ));
            }
            if !has_host && !authority.is_empty() {
                hdrs.push((
                    Bytes::from_static(b"host"),
                    Bytes::copy_from_slice(authority),
                ));
            }
            let req = crate::api::dispatch::build_request(
                Bytes::copy_from_slice(method),
                Bytes::copy_from_slice(path),
                hdrs,
                Bytes::new(),
                self.client_ip.as_str(),
                crate::api::request::HttpVersion::Http3,
            );
            return Decision::StreamHandler(Box::new(H3HandlerStream {
                entry: entry.clone(),
                req,
                max_request_body: crate::api::app::max_body_size() as u64,
                log: H3AccessLog {
                    method: Bytes::copy_from_slice(method),
                    authority: Bytes::copy_from_slice(authority),
                    path: Bytes::copy_from_slice(path),
                    user_agent: Bytes::copy_from_slice(user_agent),
                    client_ip: self.client_ip,
                },
            }));
        }

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

        // F-171: h2c 上流は多重化接続（F-174）のストリームで全二重に中継する（gRPC を含む。
        // トレーラーは RespMsg::Trailers）。http2 feature が無ければ従来どおりバッファ経路。
        let h2_upstream = server.target.use_h2c || upstream_group.use_h2c();
        #[cfg(not(feature = "http2"))]
        if h2_upstream {
            return Decision::Buffer;
        }
        // F-175: HTTPS 上流は ALPN で HTTP/2 を試す（HTTP/1.1 しか話さないと分かっている上流は除く）。
        // 試す場合も、上流が HTTP/1.1 を選んだときのために HTTP/1.1 の要求 head を作っておく。
        #[cfg(feature = "http2")]
        let h2_tls = server.target.h2_over_tls() && {
            let addr = server.target.conn_addr();
            let key = crate::http_utils::PoolKeyStr::tls_addr(
                addr.as_str(),
                server.target.sni(),
                upstream_group.tls_profile(),
            );
            !crate::http2::upstream_mux::https_known_h1(key.as_str())
        };
        #[cfg(not(feature = "http2"))]
        let h2_tls = false;
        // gRPC を HTTP/1.1 上流へ送る構成はバッファ経路（従来どおり）。
        if is_grpc && !h2_upstream && !h2_tls {
            return Decision::Buffer;
        }

        // --- リクエスト head 構築 ---
        let client_encoding = accept_encoding
            .map(AcceptedEncoding::parse)
            .unwrap_or(AcceptedEncoding::Identity);
        let compression = resolve_http3_compression_config(&path_compression, &config.http3_config);
        let added = crate::proxy::expanded_added_headers(
            security,
            self.client_ip.as_str(),
            authority,
            std::str::from_utf8(path).unwrap_or("/"),
        );
        #[cfg(feature = "http2")]
        let h2 = (h2_upstream || h2_tls).then(|| {
            // gRPC はサービス/メソッドのフルパスを保持する（B-39）。
            let final_path = compute_upstream_request_path(
                std::str::from_utf8(path).unwrap_or("/"),
                &prefix,
                &server.target.path_prefix,
                is_grpc,
            );
            crate::http3_stream::H2Upstream {
                method: Bytes::copy_from_slice(method),
                path: Bytes::from(final_path),
                authority: Bytes::copy_from_slice(server.target.host.as_bytes()),
                headers: pack_h3_request_headers(headers, security, &added),
                connect_timeout: Duration::from_secs(security.backend_connect_timeout_secs),
            }
        });
        #[cfg(feature = "http2")]
        let request_head = if h2_upstream {
            Vec::new()
        } else {
            let final_path = compute_backend_path(&server.target, path, &prefix);
            build_h1_request_head(
                &server.target,
                method,
                &final_path,
                headers,
                security,
                &added,
            )
        };
        #[cfg(not(feature = "http2"))]
        let request_head = {
            let final_path = compute_backend_path(&server.target, path, &prefix);
            build_h1_request_head(
                &server.target,
                method,
                &final_path,
                headers,
                security,
                &added,
            )
        };

        // F-44: TLS バックエンドもストリーミング対象（バックエンドタスクが全二重 TLS で貫通）。
        let use_tls = server.target.use_tls;
        let sni = server.target.sni().to_string();
        let tls_profile = upstream_group.tls_profile();

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
            tls_profile,
            pool_max_idle: security.max_idle_connections_per_host,
            pool_idle_timeout_secs: security.idle_connection_timeout_secs,
            no_response_body: method.eq_ignore_ascii_case(b"HEAD"),
            retry_idempotent: crate::http_utils::is_idempotent_method(method),
            #[cfg(feature = "http2")]
            h2,
        })
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
        conn: &mut H3Conn,
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
            let head_present_before = pr.head.is_some();
            if Self::try_flush_partial(h3_conn, &mut self.conn, stream_id, pr) {
                completed.push(stream_id);
                did_work = true;
            } else if pr.head.is_some() != head_present_before {
                // 未完了でもヘッダを送れた＝進捗あり。本文が一部だけ進んだのは、送れる量を使い切った
                // ということなので再投入しない（handle_writable_streams・streaming.rs の Partial と同じ。v0.8.0）
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
                let head_present_before = pr.head.is_some();
                if Self::try_flush_partial(h3_conn, &mut self.conn, stream_id, pr) {
                    completed.push(stream_id);
                    did_work = true;
                } else if pr.head.is_some() != head_present_before {
                    did_work = true;
                }
                // 本文が一部だけ進んだ（`pr.written` が増えた）だけなら再投入しない: 送れる量を使い切っており、
                // 続きは ACK・MAX_DATA の受信かタイマーでしか送れない（streaming.rs の Partial と同じ。v0.8.0）
            }
        }
        for stream_id in completed {
            self.partial_responses.remove(&stream_id);
        }

        Ok(did_work)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 本文のバッファの分割はコピーせず（同じ領域を指したまま）、前半・後半に分かれる。
    #[test]
    fn zc_buf_split_keeps_the_same_memory() {
        use quiche::{BufFactory, BufSplit};
        let body = Bytes::from(vec![7u8; 4096]);
        let mut head = ZcBuf(body.slice(100..));
        let tail = head.split_at(1000);
        assert_eq!(head.as_ref().len(), 1000);
        assert_eq!(tail.as_ref().len(), 4096 - 100 - 1000);
        assert_eq!(head.as_ref().as_ptr(), body[100..].as_ptr());
        assert_eq!(tail.as_ref().as_ptr(), body[1100..].as_ptr());
        // スライスで渡される経路は 1 回コピーする
        let copied = ZcFactory::buf_from_slice(b"abc");
        assert_eq!(copied.as_ref(), b"abc");
    }

    /// F-186: 接続が減ってアイドルになった表は縮み、小さい表・まだ埋まっている表は縮まない。
    #[test]
    fn idle_conn_map_shrinks_only_when_mostly_empty() {
        let mut conns: ConnMapInner = HashMap::with_hasher(conn_map_hasher());
        conns.reserve(8192);
        let peak = conns.capacity();
        shrink_idle_conn_map(&mut conns);
        assert!(
            conns.capacity() < peak / 4,
            "空の表が縮んでいない: {}",
            conns.capacity()
        );
        assert!(conns.capacity() >= CONN_MAP_SHRINK_MIN_CAPACITY);

        // 下限以下の表は触らない。
        let small = conns.capacity();
        shrink_idle_conn_map(&mut conns);
        assert_eq!(conns.capacity(), small);
    }

    fn check(fields: &[(&str, &str)]) -> Result<H3FieldSection, &'static str> {
        h3_check_field_section(fields.iter().map(|(n, v)| (n.as_bytes(), v.as_bytes())))
    }

    const GOOD: [(&str, &str); 4] = [
        (":method", "GET"),
        (":scheme", "https"),
        (":authority", "example.com"),
        (":path", "/"),
    ];

    /// F-236 の F2: HTTP/1.1 の上流へ渡すと要求の密輸になるフィールドを拒否する
    #[test]
    fn test_h3_field_section_rejects_smuggling_fields() {
        let with = |n: &'static str, v: &'static str| {
            let mut f = GOOD.to_vec();
            f.push((n, v));
            check(&f)
        };
        assert!(with("x-a", "fine").is_ok());
        assert!(with("x-a", "v\r\nx-injected: 1").is_err());
        assert!(with("x-a", "v\0").is_err());
        assert!(with("x-a", " lead").is_err());
        assert!(with("x a", "v").is_err());
        let mut bad_path = GOOD.to_vec();
        bad_path[3] = (":path", "/a b");
        assert!(check(&bad_path).is_err());
        let mut bad_method = GOOD.to_vec();
        bad_method[0] = (":method", "GET /x");
        assert!(check(&bad_method).is_err());
    }

    #[test]
    fn test_h3_field_section_accepts_valid_request_and_trailers() {
        assert_eq!(check(&GOOD), Ok(H3FieldSection::Request));
        let mut with_regular = GOOD.to_vec();
        with_regular.push(("user-agent", "x"));
        with_regular.push(("te", "trailers"));
        assert_eq!(check(&with_regular), Ok(H3FieldSection::Request));
        // :authority は Host で代替できる。
        assert_eq!(
            check(&[
                (":method", "GET"),
                (":scheme", "https"),
                (":path", "/"),
                ("host", "example.com"),
            ]),
            Ok(H3FieldSection::Request)
        );
        assert_eq!(
            check(&[(":method", "CONNECT"), (":authority", "example.com:443")]),
            Ok(H3FieldSection::Request)
        );
        assert_eq!(check(&[("grpc-status", "0")]), Ok(H3FieldSection::NoPseudo));
        assert_eq!(check(&[]), Ok(H3FieldSection::NoPseudo));
    }

    #[test]
    fn test_h3_field_section_rejects_malformed_requests() {
        // h3spec: duplicated / absent / prohibited / after regular fields
        let mut dup = GOOD.to_vec();
        dup.insert(1, (":method", "GET"));
        assert!(check(&dup).is_err());
        assert!(check(&[(":scheme", "https"), (":path", "/")]).is_err());
        assert!(check(&[(":method", "GET"), (":path", "/")]).is_err());
        // https なのに :authority も Host も無い
        assert!(check(&[(":method", "GET"), (":scheme", "https"), (":path", "/")]).is_err());
        assert!(check(&[(":method", "GET"), (":scheme", "https")]).is_err());
        let mut status = GOOD.to_vec();
        status.push((":status", "200"));
        assert!(check(&status).is_err());
        let mut protocol = GOOD.to_vec();
        protocol.push((":protocol", "websocket"));
        assert!(check(&protocol).is_err());
        assert!(check(&[
            (":method", "GET"),
            (":scheme", "https"),
            ("user-agent", "x"),
            (":path", "/"),
        ])
        .is_err());
        // その他の malformed
        let mut empty_path = GOOD.to_vec();
        empty_path[3] = (":path", "");
        assert!(check(&empty_path).is_err());
        for bad in [
            ("Host", "example.com"),
            ("connection", "close"),
            ("transfer-encoding", "chunked"),
            ("te", "gzip"),
        ] {
            let mut v = GOOD.to_vec();
            v.push(bad);
            assert!(check(&v).is_err(), "{bad:?} must be rejected");
        }
        assert!(check(&[
            (":method", "CONNECT"),
            (":scheme", "https"),
            (":authority", "a")
        ])
        .is_err());
        assert!(check(&[(":method", "CONNECT")]).is_err());
    }

    #[test]
    fn test_b94_derive_server_cid_is_deterministic_and_keyed() {
        let key = hmac::Key::new(hmac::HMAC_SHA256, b"0123456789abcdef0123456789abcdef");
        let other = hmac::Key::new(hmac::HMAC_SHA256, b"fedcba9876543210fedcba9876543210");
        let a = derive_server_cid(&key, b"client-dcid-1");
        assert_eq!(a, derive_server_cid(&key, b"client-dcid-1"));
        assert_ne!(a, derive_server_cid(&key, b"client-dcid-2"));
        assert_ne!(a, derive_server_cid(&other, b"client-dcid-1"));
        assert_eq!(a.len(), quiche::MAX_CONN_ID_LEN);
    }

    /// B-94: 同じ元 DCID の Initial が 2 回届いても（クライアントの PTO 再送・複数パケットに
    /// またがる ClientHello）新規接続は 1 つだけ作られる。修正前は届くたびに乱数 SCID で
    /// 別接続を `accept` していた。
    #[test]
    fn test_b94_retransmitted_initial_maps_to_same_connection() {
        // tests/fixtures はコンテナビルドのコンテキストに含まれないため、その場で生成する。
        let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert = ck.cert.pem();
        let key = ck.signing_key.serialize_pem();
        let mut server_cfg =
            new_quic_config_with_certs(cert.as_bytes(), key.as_bytes()).expect("server config");
        server_cfg
            .set_application_protos(h3::APPLICATION_PROTOCOL)
            .unwrap();
        server_cfg.set_initial_max_data(1 << 20);
        let quic_config = Rc::new(RefCell::new(server_cfg));

        let mut client_cfg = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
        client_cfg
            .set_application_protos(h3::APPLICATION_PROTOCOL)
            .unwrap();
        client_cfg.verify_peer(false);
        let client_scid = ConnectionId::from_ref(&[7u8; 16]);
        let client_addr: SocketAddr = "127.0.0.1:40000".parse().unwrap();
        let server_addr: SocketAddr = "127.0.0.1:4433".parse().unwrap();
        let mut client = quiche::connect(
            Some("localhost"),
            &client_scid,
            client_addr,
            server_addr,
            &mut client_cfg,
        )
        .unwrap();
        let mut initial = vec![0u8; 1500];
        let (len, _) = client.send(&mut initial).unwrap();
        initial.truncate(len);

        let mut conns: ConnMapInner = HashMap::with_hasher(conn_map_hasher());
        let notify = crate::http3_stream::H3Notify::new();
        let spawner = crate::http3_stream::backend_task_spawner();
        let wake_queue: crate::http3_stream::WakeQueue = Default::default();
        let mut dirty = VecDeque::new();
        let key = hmac::Key::new(hmac::HMAC_SHA256, &[42u8; 32]);
        for round in 0..2 {
            if round == 1 {
                // 1 回目で作られた接続に目印を付ける（作り直されると消える）。
                let h = conns.values_mut().next().unwrap();
                h.stream_bodies.insert(u64::MAX, Default::default());
            }
            let mut pkt = initial.clone();
            process_datagram_segments(
                &mut conns,
                &mut pkt,
                client_addr,
                None,
                &key,
                &quic_config,
                server_addr,
                &notify,
                &spawner,
                &wake_queue,
                &mut dirty,
            )
            .unwrap();
        }
        assert_eq!(
            conns.len(),
            1,
            "retransmitted Initial must not create a 2nd connection"
        );
        // 2 回目は**既存の接続へ**届いていること（同じキーで作り直して最初の
        // ハンドシェイク状態を捨てていないこと）。
        let handler = conns.values().next().unwrap();
        assert!(
            handler.stream_bodies.contains_key(&u64::MAX),
            "the 2nd datagram must be delivered to the existing connection"
        );
    }

    /// F-152: `[http3] recv_drain_max`（reactor 経路の 1 イテレーションあたり
    /// データグラム drain 上限）の既定値とクランプ範囲。
    ///
    /// 既定値は設定側の既定（`default_h3_recv_drain_max`）と同値であること
    /// （v0.8.0 で 64 → 1024）。
    #[test]
    fn test_recv_drain_max_default_and_clamp() {
        assert_eq!(
            H3_RECV_DRAIN_MAX_DEFAULT, 1024,
            "既定値は設定の既定（default_h3_recv_drain_max）と同値であること"
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
        assert_eq!(config.max_udp_payload_size, 1452);
        assert!(config.discover_pmtu);
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

    /// B-97: 応答ヘッダを 1 回の確保に詰める（`:status` を除き、content-length を足す）。
    #[test]
    fn test_b97_pack_resp_headers() {
        let packed = pack_resp_headers(
            &[
                (b":status", b"200"),
                (b"content-type", b"text/plain"),
                (b"x-empty", b""),
            ],
            Some(1234),
        );
        let pairs: Vec<(&[u8], &[u8])> = packed
            .iter()
            .map(|(n, v)| (n.as_ref(), v.as_ref()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                (&b"content-type"[..], &b"text/plain"[..]),
                (&b"x-empty"[..], &b""[..]),
                (&b"content-length"[..], &b"1234"[..]),
            ]
        );
        assert!(pack_resp_headers(&[], None).is_empty());
        let only_cl = pack_resp_headers(&[], Some(0));
        assert_eq!(only_cl.len(), 1);
        assert_eq!(only_cl[0].1.as_ref(), b"0");
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
