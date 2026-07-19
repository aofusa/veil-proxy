//! # FreeBSD kTLS 送受信オフロード実装（F-126）
//!
//! FreeBSD カーネルの kTLS 機能（`TCP_TXTLS_ENABLE` / `TCP_RXTLS_ENABLE`）を
//! 直接使用するための実装。Linux 版（[`crate::ktls`]）とは ABI・API の形が
//! 異なるため完全に別モジュールとして隔離し、Linux 経路には一切影響しない。
//!
//! ## Linux との API 差分
//!
//! | ステップ | Linux | FreeBSD |
//! |----------|-------|---------|
//! | ULP 設定 | `setsockopt(TCP_ULP, "tls")` | 不要（[`crate::ktls::setup_ulp`] の FreeBSD 版が no-op） |
//! | TX 有効化 | `setsockopt(SOL_TLS, TLS_TX, &tls12_crypto_info)` | `setsockopt(IPPROTO_TCP, TCP_TXTLS_ENABLE, &tls_enable)` |
//! | RX 有効化 | `setsockopt(SOL_TLS, TLS_RX, &tls12_crypto_info)` | `setsockopt(IPPROTO_TCP, TCP_RXTLS_ENABLE, &tls_enable)` |
//!
//! `struct tls_enable` のフィールド順・型・関連定数値は FreeBSD 14.3-RELEASE
//! 実機（`/usr/include/sys/ktls.h` / `/usr/include/netinet/tcp.h` /
//! `/usr/include/crypto/cryptodev.h`）のヘッダを SSOT として確認済み
//! （`size_of::<TlsEnable>() == 64` を [`tests::test_tls_enable_layout`] で担保）。

use std::io;
use std::os::unix::io::RawFd;

use crate::ktls::{TlsKeyMaterial, TLS_1_2_VERSION};

// ====================
// FreeBSD カーネル定数
// ====================
// FreeBSD 14.3-RELEASE 実機ヘッダで確認済み:
//   - /usr/include/netinet/tcp.h: TCP_TXTLS_ENABLE / TCP_RXTLS_ENABLE
//   - /usr/include/sys/ktls.h: TLS_MAJOR_VER_ONE / TLS_MINOR_VER_TWO / TLS_MINOR_VER_THREE
//   - /usr/include/crypto/cryptodev.h: CRYPTO_AES_NIST_GCM_16 (= 25。設計時の目安値 26 は誤りだったため実機値へ訂正)

/// TX 方向 kTLS 有効化オプション（`TCP_TXTLS_ENABLE`）
const TCP_TXTLS_ENABLE: libc::c_int = 39;

/// RX 方向 kTLS 有効化オプション（`TCP_RXTLS_ENABLE`）
const TCP_RXTLS_ENABLE: libc::c_int = 41;

/// AES-GCM（16 byte ICV）暗号アルゴリズム（`CRYPTO_AES_NIST_GCM_16`）
const CRYPTO_AES_NIST_GCM_16: libc::c_int = 25;

/// TLS メジャーバージョン（常に 3、`TLS_MAJOR_VER_ONE`）
const TLS_MAJOR_VER_ONE: u8 = 3;

/// TLS 1.2 マイナーバージョン（`TLS_MINOR_VER_TWO`）
const TLS_MINOR_VER_TWO: u8 = 3;

/// TLS 1.3 マイナーバージョン（`TLS_MINOR_VER_THREE`）
const TLS_MINOR_VER_THREE: u8 = 4;

// ====================
// struct tls_enable（FreeBSD 14.3 ABI）
// ====================

/// FreeBSD `struct tls_enable`（`/usr/include/sys/ktls.h`）と同一レイアウト。
///
/// ポインタ渡しの `setsockopt` のため、`cipher_key` / `iv` が指すバッファは
/// この構造体を使い終える（`setsockopt` 呼び出し完了）まで生存させる必要がある
/// （[`enable`] 内でスタックローカルのまま完結させ、ヒープ確保は行わない）。
#[repr(C)]
struct TlsEnable {
    cipher_key: *const u8,
    iv: *const u8,
    auth_key: *const u8,
    cipher_algorithm: libc::c_int,
    cipher_key_len: libc::c_int,
    iv_len: libc::c_int,
    auth_algorithm: libc::c_int,
    auth_key_len: libc::c_int,
    flags: libc::c_int,
    tls_vmajor: u8,
    tls_vminor: u8,
    rec_seq: [u8; 8],
}

/// TX または RX の kTLS オフロードを有効化する共通処理。
///
/// `material` の salt/iv は TLS バージョンにより FreeBSD へ渡す `iv` フィールドの
/// 組み立て方が異なる（FreeBSD 14 実機で確認した設計方針）:
/// - TLS 1.2: `iv` = salt（4 バイト、implicit IV のみ）、`iv_len = 4`
/// - TLS 1.3: `iv` = salt(4) ++ iv(8) の連結（12 バイト、rustls の 1.3 IV 全体）、`iv_len = 12`
fn enable(fd: RawFd, direction: libc::c_int, material: &TlsKeyMaterial) -> io::Result<()> {
    let tls_vminor = if material.version == TLS_1_2_VERSION {
        TLS_MINOR_VER_TWO
    } else {
        TLS_MINOR_VER_THREE
    };

    // TLS 1.2: salt(4) のみ。TLS 1.3: salt(4)++iv(8) の連結 12 バイト。
    // combined_iv はこの関数のスタックフレーム内で setsockopt 呼び出し完了まで生存する。
    let mut combined_iv = [0u8; 12];
    let (iv_ptr, iv_len): (*const u8, usize) = if material.version == TLS_1_2_VERSION {
        (material.salt.as_ptr(), material.salt.len())
    } else {
        combined_iv[0..4].copy_from_slice(&material.salt);
        combined_iv[4..12].copy_from_slice(&material.iv);
        (combined_iv.as_ptr(), combined_iv.len())
    };

    let tls_enable = TlsEnable {
        cipher_key: material.key.as_ptr(),
        iv: iv_ptr,
        auth_key: std::ptr::null(),
        cipher_algorithm: CRYPTO_AES_NIST_GCM_16,
        cipher_key_len: material.key_len as libc::c_int,
        iv_len: iv_len as libc::c_int,
        auth_algorithm: 0,
        auth_key_len: 0,
        flags: 0,
        tls_vmajor: TLS_MAJOR_VER_ONE,
        tls_vminor,
        rec_seq: material.rec_seq,
    };

    // SAFETY: tls_enable.cipher_key は material.key（呼び出し側フレームで生存）を、
    // tls_enable.iv は material.salt または combined_iv（本関数のスタックフレームで
    // 生存）を指す。setsockopt はカーネルが optval を読み取りコピーするだけの
    // read-only 呼び出しであり、呼び出しが返るまでの間これらのバッファは
    // 有効なメモリを指し続ける。
    let result = unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            direction,
            &tls_enable as *const TlsEnable as *const libc::c_void,
            std::mem::size_of::<TlsEnable>() as libc::socklen_t,
        )
    };

    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// TX 方向の kTLS オフロードを有効化する。
pub fn enable_tx(fd: RawFd, material: &TlsKeyMaterial) -> io::Result<()> {
    enable(fd, TCP_TXTLS_ENABLE, material)
}

/// RX 方向の kTLS オフロードを有効化する。
pub fn enable_rx(fd: RawFd, material: &TlsKeyMaterial) -> io::Result<()> {
    enable(fd, TCP_RXTLS_ENABLE, material)
}

// ====================
// テスト
// ====================

#[cfg(test)]
mod tests {
    use super::*;

    /// FreeBSD 14.3-RELEASE 実機（`/usr/include/sys/ktls.h`）で
    /// `sizeof(struct tls_enable) == 64` / 各フィールドオフセットを
    /// 確認済み。Rust 側の `#[repr(C)]` 定義が同一レイアウトになることを
    /// コンパイル時（`offset_of!`）に検証する。
    #[test]
    fn test_tls_enable_layout() {
        assert_eq!(std::mem::size_of::<TlsEnable>(), 64);
        assert_eq!(std::mem::align_of::<TlsEnable>(), 8);

        assert_eq!(std::mem::offset_of!(TlsEnable, cipher_key), 0);
        assert_eq!(std::mem::offset_of!(TlsEnable, iv), 8);
        assert_eq!(std::mem::offset_of!(TlsEnable, auth_key), 16);
        assert_eq!(std::mem::offset_of!(TlsEnable, cipher_algorithm), 24);
        assert_eq!(std::mem::offset_of!(TlsEnable, cipher_key_len), 28);
        assert_eq!(std::mem::offset_of!(TlsEnable, iv_len), 32);
        assert_eq!(std::mem::offset_of!(TlsEnable, auth_algorithm), 36);
        assert_eq!(std::mem::offset_of!(TlsEnable, auth_key_len), 40);
        assert_eq!(std::mem::offset_of!(TlsEnable, flags), 44);
        assert_eq!(std::mem::offset_of!(TlsEnable, tls_vmajor), 48);
        assert_eq!(std::mem::offset_of!(TlsEnable, tls_vminor), 49);
        assert_eq!(std::mem::offset_of!(TlsEnable, rec_seq), 50);
    }

    #[test]
    fn test_material_secure_clear() {
        let mut material = TlsKeyMaterial {
            key: [1u8; 32],
            key_len: 16,
            salt: [1, 2, 3, 4],
            iv: [1; 8],
            rec_seq: [1; 8],
            version: TLS_1_2_VERSION,
        };
        material.secure_clear();
        assert!(material.key.iter().all(|&b| b == 0));
        assert!(material.salt.iter().all(|&b| b == 0));
        assert!(material.iv.iter().all(|&b| b == 0));
        assert!(material.rec_seq.iter().all(|&b| b == 0));
    }
}
