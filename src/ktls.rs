//! # kTLS (Kernel TLS) 自前実装モジュール
//!
//! Linux カーネルの kTLS 機能を直接使用するための実装。
//! ktls2 クレートを置き換え、依存なしで kTLS を有効化します。
//!
//! ## サポートする暗号スイート
//! - AES-128-GCM (TLS 1.2/1.3)
//! - AES-256-GCM (TLS 1.2/1.3)
//!
//! ## 使用方法
//! 1. TLS ハンドシェイク完了後に `dangerous_extract_secrets()` で鍵を抽出
//! 2. `extract_tx_rx()` で TX/RX 用の `CryptoInfo` を生成
//! 3. `setsockopt` で ULP 設定後、TX/RX 情報をカーネルに渡す
//! 4. `secure_clear()` で鍵をメモリからゼロ化

use std::io;
use std::os::unix::io::RawFd;

use rustls::{ConnectionTrafficSecrets, ExtractedSecrets, ProtocolVersion};

// ====================
// Linux カーネル定数
// ====================
// include/uapi/linux/tls.h より

/// TLS ソケットオプションレベル
pub const SOL_TLS: libc::c_int = 282;

/// TCP ULP オプション (TCP_ULP = 31)
pub const TCP_ULP: libc::c_int = 31;

/// TLS 送信方向
pub const TLS_TX: libc::c_int = 1;

/// TLS 受信方向
pub const TLS_RX: libc::c_int = 2;

/// TLS 1.2 バージョン (0x0303)
pub const TLS_1_2_VERSION: u16 = 0x0303;

/// TLS 1.3 バージョン (0x0304)
pub const TLS_1_3_VERSION: u16 = 0x0304;

/// AES-GCM-128 暗号タイプ
pub const TLS_CIPHER_AES_GCM_128: u16 = 51;

/// AES-GCM-256 暗号タイプ
pub const TLS_CIPHER_AES_GCM_256: u16 = 52;

// フィールドサイズ定数
const TLS_CIPHER_AES_GCM_128_IV_SIZE: usize = 8;
const TLS_CIPHER_AES_GCM_128_KEY_SIZE: usize = 16;
const TLS_CIPHER_AES_GCM_128_SALT_SIZE: usize = 4;
const TLS_CIPHER_AES_GCM_128_REC_SEQ_SIZE: usize = 8;

const TLS_CIPHER_AES_GCM_256_IV_SIZE: usize = 8;
const TLS_CIPHER_AES_GCM_256_KEY_SIZE: usize = 32;
const TLS_CIPHER_AES_GCM_256_SALT_SIZE: usize = 4;
const TLS_CIPHER_AES_GCM_256_REC_SEQ_SIZE: usize = 8;

// ====================
// Linux カーネル構造体
// ====================

/// tls_crypto_info ヘッダー (全暗号スイート共通)
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TlsCryptoInfoHeader {
    /// TLS バージョン (TLS_1_2_VERSION または TLS_1_3_VERSION)
    pub version: u16,
    /// 暗号タイプ (TLS_CIPHER_AES_GCM_128 等)
    pub cipher_type: u16,
}

/// tls12_crypto_info_aes_gcm_128
#[repr(C)]
pub struct Tls12CryptoInfoAesGcm128 {
    /// 共通ヘッダー
    pub info: TlsCryptoInfoHeader,
    /// Explicit IV (8 bytes) - rustls IV[4..12]
    pub iv: [u8; TLS_CIPHER_AES_GCM_128_IV_SIZE],
    /// 暗号鍵 (16 bytes)
    pub key: [u8; TLS_CIPHER_AES_GCM_128_KEY_SIZE],
    /// Salt/Implicit IV (4 bytes) - rustls IV[0..4]
    pub salt: [u8; TLS_CIPHER_AES_GCM_128_SALT_SIZE],
    /// レコードシーケンス番号 (8 bytes, big-endian)
    pub rec_seq: [u8; TLS_CIPHER_AES_GCM_128_REC_SEQ_SIZE],
}

/// tls12_crypto_info_aes_gcm_256
#[repr(C)]
pub struct Tls12CryptoInfoAesGcm256 {
    /// 共通ヘッダー
    pub info: TlsCryptoInfoHeader,
    /// Explicit IV (8 bytes) - rustls IV[4..12]
    pub iv: [u8; TLS_CIPHER_AES_GCM_256_IV_SIZE],
    /// 暗号鍵 (32 bytes)
    pub key: [u8; TLS_CIPHER_AES_GCM_256_KEY_SIZE],
    /// Salt/Implicit IV (4 bytes) - rustls IV[0..4]
    pub salt: [u8; TLS_CIPHER_AES_GCM_256_SALT_SIZE],
    /// レコードシーケンス番号 (8 bytes, big-endian)
    pub rec_seq: [u8; TLS_CIPHER_AES_GCM_256_REC_SEQ_SIZE],
}

// ====================
// CryptoInfo 統一型
// ====================

/// kTLS 暗号化情報 (TX/RX 両方に使用)
pub enum CryptoInfo {
    /// AES-128-GCM
    Aes128Gcm(Tls12CryptoInfoAesGcm128),
    /// AES-256-GCM
    Aes256Gcm(Tls12CryptoInfoAesGcm256),
}

impl CryptoInfo {
    /// カーネル setsockopt 用のポインタを取得
    #[inline]
    pub fn as_ptr(&self) -> *const libc::c_void {
        match self {
            CryptoInfo::Aes128Gcm(info) => info as *const _ as *const libc::c_void,
            CryptoInfo::Aes256Gcm(info) => info as *const _ as *const libc::c_void,
        }
    }

    /// 構造体サイズを取得 (setsockopt の optlen 用)
    #[inline]
    pub fn size(&self) -> usize {
        match self {
            CryptoInfo::Aes128Gcm(_) => std::mem::size_of::<Tls12CryptoInfoAesGcm128>(),
            CryptoInfo::Aes256Gcm(_) => std::mem::size_of::<Tls12CryptoInfoAesGcm256>(),
        }
    }

    /// 鍵データをセキュアにゼロ化
    ///
    /// setsockopt 後に呼び出し、メモリ上の鍵を消去します。
    /// volatile 書き込みとメモリバリアにより最適化を防止。
    pub fn secure_clear(&mut self) {
        match self {
            CryptoInfo::Aes128Gcm(info) => {
                secure_zero(&mut info.key);
                secure_zero(&mut info.salt);
                secure_zero(&mut info.iv);
                secure_zero(&mut info.rec_seq);
            }
            CryptoInfo::Aes256Gcm(info) => {
                secure_zero(&mut info.key);
                secure_zero(&mut info.salt);
                secure_zero(&mut info.iv);
                secure_zero(&mut info.rec_seq);
            }
        }
    }
}

// ====================
// エラー型
// ====================

/// kTLS エラー
#[derive(Debug)]
pub enum KtlsError {
    /// 鍵長が不正
    InvalidKeyLength,
    /// IV 長が不正
    InvalidIvLength,
    /// サポートされていない暗号スイート
    UnsupportedCipher,
    /// サポートされていない TLS バージョン
    UnsupportedProtocol,
    /// シークレット抽出失敗
    SecretExtractionFailed,
    /// setsockopt 失敗
    SetsockoptFailed(i32),
    /// バッファドレイン失敗
    DrainError(String),
    /// バッファサイズ超過 (64KB 制限)
    BufferOverflow,
}

impl std::fmt::Display for KtlsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KtlsError::InvalidKeyLength => write!(f, "Invalid key length"),
            KtlsError::InvalidIvLength => write!(f, "Invalid IV length"),
            KtlsError::UnsupportedCipher => write!(f, "Unsupported cipher suite"),
            KtlsError::UnsupportedProtocol => write!(f, "Unsupported TLS protocol version"),
            KtlsError::SecretExtractionFailed => write!(f, "Failed to extract TLS secrets"),
            KtlsError::SetsockoptFailed(errno) => write!(f, "setsockopt failed: errno={}", errno),
            KtlsError::DrainError(msg) => write!(f, "Buffer drain failed: {}", msg),
            KtlsError::BufferOverflow => write!(f, "Excessive buffered data in rustls (>64KB)"),
        }
    }
}

impl std::error::Error for KtlsError {}

impl From<KtlsError> for io::Error {
    fn from(e: KtlsError) -> Self {
        io::Error::new(io::ErrorKind::Other, e.to_string())
    }
}

// ====================
// バッファドレイン
// ====================

/// ドレインバッファの最大サイズ (64KB)
/// セキュリティ制限: 過度なメモリ使用を防止
pub const MAX_DRAIN_BUFFER_SIZE: usize = 65536;

/// rustls バッファから残存平文データを抽出
///
/// kTLS 有効化前に rustls が復号済みのデータをドレインし、
/// カーネルハンドオフ後もアプリケーションに正しく渡せるようにします。
///
/// # 引数
/// * `reader` - rustls::ServerConnection または ClientConnection の reader()
///
/// # 戻り値
/// ドレインされた平文データ (通常は空)
///
/// # エラー
/// バッファが 64KB を超える場合は BufferOverflow エラー
pub fn drain_rustls_plaintext<R: std::io::Read>(reader: &mut R) -> Result<Vec<u8>, KtlsError> {
    let mut drained = Vec::with_capacity(4096);
    let mut buf = [0u8; 4096];

    loop {
        match reader.read(&mut buf) {
            Ok(0) => break, // バッファ空または EOF
            Ok(n) => {
                drained.extend_from_slice(&buf[..n]);
                // セキュリティ制限: 64KB 以上は異常
                if drained.len() > MAX_DRAIN_BUFFER_SIZE {
                    return Err(KtlsError::BufferOverflow);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(KtlsError::DrainError(e.to_string())),
        }
    }

    Ok(drained)
}

// ====================
// TCP_CORK 最適化
// ====================

/// TCP_CORK を設定 (TCP セグメントのバッチ処理)
///
/// kTLS 設定中に小さな TCP パケットの送信を遅延し、
/// 効率的なネットワーク転送を実現します。
///
/// # 引数
/// * `fd` - ソケットファイルディスクリプタ
/// * `enable` - true で CORK 有効化、false で無効化
pub fn set_tcp_cork(fd: RawFd, enable: bool) -> io::Result<()> {
    const TCP_CORK: libc::c_int = 3;
    let val: libc::c_int = if enable { 1 } else { 0 };

    let result = unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            TCP_CORK,
            &val as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };

    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

// ====================
// kTLS 互換暗号スイート
// ====================

/// kTLS 互換の暗号スイートを返す
///
/// Linux カーネル kTLS がサポートする暗号スイートのみを返します。
/// TLS 設定時にこのリストを使用することで、kTLS 非互換の暗号が
/// ネゴシエートされることを防ぎます。
///
/// # サポート暗号スイート
/// - TLS 1.3: AES-128-GCM, AES-256-GCM
/// - TLS 1.2: ECDHE-RSA-AES-128-GCM, ECDHE-RSA-AES-256-GCM
///
/// # 注意
/// ChaCha20-Poly1305 は Linux 5.11+ が必要なため含まれていません。
pub fn ktls_compatible_cipher_suites() -> Vec<rustls::SupportedCipherSuite> {
    use rustls::crypto::aws_lc_rs::cipher_suite;
    vec![
        // TLS 1.3 AES-GCM
        cipher_suite::TLS13_AES_128_GCM_SHA256,
        cipher_suite::TLS13_AES_256_GCM_SHA384,
        // TLS 1.2 ECDHE-RSA-AES-GCM
        cipher_suite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
        cipher_suite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
        // TLS 1.2 ECDHE-ECDSA-AES-GCM (EC 証明書用)
        cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
        cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
    ]
}

// ====================
// セキュアゼロ化
// ====================

/// セキュアなバイト配列のゼロ化
///
/// volatile 書き込みとメモリバリアでコンパイラ最適化を防止。
#[inline]
fn secure_zero(data: &mut [u8]) {
    for byte in data.iter_mut() {
        unsafe {
            std::ptr::write_volatile(byte, 0);
        }
    }
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
}

// ====================
// シークレット抽出
// ====================

/// TLS バージョンをカーネル形式に変換
#[inline]
fn get_tls_version(version: Option<ProtocolVersion>) -> Result<u16, KtlsError> {
    match version {
        Some(ProtocolVersion::TLSv1_2) => Ok(TLS_1_2_VERSION),
        Some(ProtocolVersion::TLSv1_3) => Ok(TLS_1_3_VERSION),
        None => Ok(TLS_1_3_VERSION), // デフォルトは TLS 1.3
        _ => Err(KtlsError::UnsupportedProtocol),
    }
}

/// AES-128-GCM 用 CryptoInfo を構築
#[inline(always)]
fn build_aes128_gcm_info(
    seq_num: u64,
    key: &rustls::crypto::cipher::AeadKey,
    iv: &rustls::crypto::cipher::Iv,
    tls_version: u16,
) -> Result<Tls12CryptoInfoAesGcm128, KtlsError> {
    // バリデーション (ログに鍵情報は出力しない)
    debug_assert!(key.as_ref().len() == 16, "AES-128-GCM key must be 16 bytes");
    debug_assert!(iv.as_ref().len() == 12, "IV must be 12 bytes");

    // 直接変換 (ヒープ割り当てなし)
    let key_bytes: [u8; 16] = key
        .as_ref()
        .try_into()
        .map_err(|_| KtlsError::InvalidKeyLength)?;
    let iv_bytes: [u8; 12] = iv
        .as_ref()
        .try_into()
        .map_err(|_| KtlsError::InvalidIvLength)?;

    // 構造体を直接構築し、IV を salt + nonce に分割
    let mut info = Tls12CryptoInfoAesGcm128 {
        info: TlsCryptoInfoHeader {
            version: tls_version,
            cipher_type: TLS_CIPHER_AES_GCM_128,
        },
        key: key_bytes,
        salt: [0u8; 4],
        iv: [0u8; 8],
        rec_seq: seq_num.to_be_bytes(),
    };
    // IV[0..4] -> salt, IV[4..12] -> iv (インライン分割)
    info.salt.copy_from_slice(&iv_bytes[0..4]);
    info.iv.copy_from_slice(&iv_bytes[4..12]);

    Ok(info)
}

/// AES-256-GCM 用 CryptoInfo を構築
#[inline(always)]
fn build_aes256_gcm_info(
    seq_num: u64,
    key: &rustls::crypto::cipher::AeadKey,
    iv: &rustls::crypto::cipher::Iv,
    tls_version: u16,
) -> Result<Tls12CryptoInfoAesGcm256, KtlsError> {
    debug_assert!(key.as_ref().len() == 32, "AES-256-GCM key must be 32 bytes");
    debug_assert!(iv.as_ref().len() == 12, "IV must be 12 bytes");

    let key_bytes: [u8; 32] = key
        .as_ref()
        .try_into()
        .map_err(|_| KtlsError::InvalidKeyLength)?;
    let iv_bytes: [u8; 12] = iv
        .as_ref()
        .try_into()
        .map_err(|_| KtlsError::InvalidIvLength)?;

    let mut info = Tls12CryptoInfoAesGcm256 {
        info: TlsCryptoInfoHeader {
            version: tls_version,
            cipher_type: TLS_CIPHER_AES_GCM_256,
        },
        key: key_bytes,
        salt: [0u8; 4],
        iv: [0u8; 8],
        rec_seq: seq_num.to_be_bytes(),
    };
    info.salt.copy_from_slice(&iv_bytes[0..4]);
    info.iv.copy_from_slice(&iv_bytes[4..12]);

    Ok(info)
}

/// 単一方向の CryptoInfo を抽出
fn extract_single(
    secrets: (u64, ConnectionTrafficSecrets),
    tls_version: u16,
) -> Result<CryptoInfo, KtlsError> {
    let (seq_num, traffic_secrets) = secrets;

    match traffic_secrets {
        ConnectionTrafficSecrets::Aes128Gcm { key, iv } => {
            let info = build_aes128_gcm_info(seq_num, &key, &iv, tls_version)?;
            Ok(CryptoInfo::Aes128Gcm(info))
        }
        ConnectionTrafficSecrets::Aes256Gcm { key, iv } => {
            let info = build_aes256_gcm_info(seq_num, &key, &iv, tls_version)?;
            Ok(CryptoInfo::Aes256Gcm(info))
        }
        _ => Err(KtlsError::UnsupportedCipher),
    }
}

/// TX/RX 両方の CryptoInfo をバッチ抽出
///
/// 共有の TLS バージョン検証を行い、TX と RX の暗号情報を一度に取得します。
///
/// # 引数
/// * `secrets` - rustls の `dangerous_extract_secrets()` から取得した ExtractedSecrets
/// * `protocol_version` - TLS プロトコルバージョン
///
/// # 戻り値
/// (TX用CryptoInfo, RX用CryptoInfo) のタプル
pub fn extract_tx_rx(
    secrets: ExtractedSecrets,
    protocol_version: Option<ProtocolVersion>,
) -> Result<(CryptoInfo, CryptoInfo), KtlsError> {
    let tls_version = get_tls_version(protocol_version)?;

    let tx = extract_single(secrets.tx, tls_version)?;
    let rx = extract_single(secrets.rx, tls_version)?;

    Ok((tx, rx))
}

// ====================
// setsockopt ヘルパー
// ====================

/// TLS ULP を設定
///
/// kTLS を有効化する前に必ず呼び出す必要があります。
pub fn setup_ulp(fd: RawFd) -> io::Result<()> {
    const SOL_TCP: libc::c_int = 6;

    let result = unsafe {
        libc::setsockopt(
            fd,
            SOL_TCP,
            TCP_ULP,
            "tls\0".as_ptr() as *const libc::c_void,
            4, // "tls\0" の長さ
        )
    };

    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// TX または RX の暗号情報をカーネルに設定
///
/// # 引数
/// * `fd` - ソケットファイルディスクリプタ
/// * `direction` - TLS_TX (1) または TLS_RX (2)
/// * `info` - CryptoInfo
pub fn setup_tls_info(fd: RawFd, direction: libc::c_int, info: &CryptoInfo) -> io::Result<()> {
    let result = unsafe {
        libc::setsockopt(
            fd,
            SOL_TLS,
            direction,
            info.as_ptr(),
            info.size() as libc::socklen_t,
        )
    };

    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

// ====================
// テスト
// ====================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_struct_sizes() {
        // Linux カーネルが期待するサイズと一致することを確認
        // tls12_crypto_info_aes_gcm_128: header(4) + iv(8) + key(16) + salt(4) + rec_seq(8) = 40
        assert_eq!(std::mem::size_of::<Tls12CryptoInfoAesGcm128>(), 40);

        // tls12_crypto_info_aes_gcm_256: header(4) + iv(8) + key(32) + salt(4) + rec_seq(8) = 56
        assert_eq!(std::mem::size_of::<Tls12CryptoInfoAesGcm256>(), 56);

        // ヘッダーサイズ
        assert_eq!(std::mem::size_of::<TlsCryptoInfoHeader>(), 4);
    }

    #[test]
    fn test_crypto_info_size() {
        let info128 = Tls12CryptoInfoAesGcm128 {
            info: TlsCryptoInfoHeader {
                version: TLS_1_3_VERSION,
                cipher_type: TLS_CIPHER_AES_GCM_128,
            },
            iv: [0; 8],
            key: [0; 16],
            salt: [0; 4],
            rec_seq: [0; 8],
        };
        let crypto = CryptoInfo::Aes128Gcm(info128);
        assert_eq!(crypto.size(), 40);

        let info256 = Tls12CryptoInfoAesGcm256 {
            info: TlsCryptoInfoHeader {
                version: TLS_1_3_VERSION,
                cipher_type: TLS_CIPHER_AES_GCM_256,
            },
            iv: [0; 8],
            key: [0; 32],
            salt: [0; 4],
            rec_seq: [0; 8],
        };
        let crypto256 = CryptoInfo::Aes256Gcm(info256);
        assert_eq!(crypto256.size(), 56);
    }

    #[test]
    fn test_secure_clear() {
        let info = Tls12CryptoInfoAesGcm128 {
            info: TlsCryptoInfoHeader {
                version: TLS_1_3_VERSION,
                cipher_type: TLS_CIPHER_AES_GCM_128,
            },
            iv: [1, 2, 3, 4, 5, 6, 7, 8],
            key: [1; 16],
            salt: [1, 2, 3, 4],
            rec_seq: [1; 8],
        };
        let mut crypto = CryptoInfo::Aes128Gcm(info);
        crypto.secure_clear();

        // ゼロ化されていることを確認
        match &crypto {
            CryptoInfo::Aes128Gcm(info) => {
                assert!(info.key.iter().all(|&b| b == 0));
                assert!(info.salt.iter().all(|&b| b == 0));
                assert!(info.iv.iter().all(|&b| b == 0));
                assert!(info.rec_seq.iter().all(|&b| b == 0));
            }
            _ => panic!("Unexpected variant"),
        }
    }

    #[test]
    fn test_tls_version_conversion() {
        assert_eq!(get_tls_version(Some(ProtocolVersion::TLSv1_2)).unwrap(), TLS_1_2_VERSION);
        assert_eq!(get_tls_version(Some(ProtocolVersion::TLSv1_3)).unwrap(), TLS_1_3_VERSION);
        assert_eq!(get_tls_version(None).unwrap(), TLS_1_3_VERSION);
    }

    #[test]
    fn test_error_display() {
        assert_eq!(format!("{}", KtlsError::InvalidKeyLength), "Invalid key length");
        assert_eq!(format!("{}", KtlsError::UnsupportedCipher), "Unsupported cipher suite");
    }
}
