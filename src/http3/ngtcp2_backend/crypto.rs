//! QUIC-TLS（aws-lc-sys + ngtcp2_crypto_boringssl）

use std::ffi::{c_void, CString};
use std::path::Path;

use aws_lc_sys::{
    SSL_CTX_free, SSL_CTX_new, SSL_CTX_set_alpn_select_cb, SSL_CTX_use_PrivateKey_file,
    SSL_CTX_use_certificate_chain_file, SSL_free, SSL_new, SSL_set_accept_state, SSL_set_ex_data,
    TLS_method, SSL, SSL_CTX, SSL_FILETYPE_PEM,
};
use ngtcp2_sys::{ngtcp2_crypto_boringssl_configure_server_context, ngtcp2_crypto_conn_ref};

use super::conn::ConnRef;

/// サーバ用 TLS コンテキスト（複数セッションで共有）
pub struct TlsContext {
    ctx: *mut SSL_CTX,
    /// ALPN コールバック用（Drop で解放）
    alpn_wire: *mut Vec<u8>,
}

// SAFETY: SSL_CTX はスレッド間共有可能（セッション生成は各接続）
unsafe impl Send for TlsContext {}
unsafe impl Sync for TlsContext {}

impl TlsContext {
    /// PEM ファイルパスからサーバ TLS コンテキストを構築（ALPN = h3）
    pub fn new_server(cert_path: &Path, key_path: &Path) -> std::io::Result<Self> {
        unsafe {
            let method = TLS_method();
            if method.is_null() {
                return Err(std::io::Error::other("TLS_method failed"));
            }
            let ctx = SSL_CTX_new(method);
            if ctx.is_null() {
                return Err(std::io::Error::other("SSL_CTX_new failed"));
            }

            if ngtcp2_crypto_boringssl_configure_server_context(ctx as *mut _) != 0 {
                SSL_CTX_free(ctx);
                return Err(std::io::Error::other(
                    "ngtcp2_crypto_boringssl_configure_server_context failed",
                ));
            }

            let cert = CString::new(cert_path.to_string_lossy().as_bytes())
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "cert path"))?;
            if SSL_CTX_use_certificate_chain_file(ctx, cert.as_ptr()) != 1 {
                SSL_CTX_free(ctx);
                return Err(std::io::Error::other(format!(
                    "load cert failed: {}",
                    cert_path.display()
                )));
            }

            let key = CString::new(key_path.to_string_lossy().as_bytes())
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "key path"))?;
            if SSL_CTX_use_PrivateKey_file(ctx, key.as_ptr(), SSL_FILETYPE_PEM) != 1 {
                SSL_CTX_free(ctx);
                return Err(std::io::Error::other(format!(
                    "load key failed: {}",
                    key_path.display()
                )));
            }

            // ALPN: 0x02 'h' '3'
            let wire = vec![2u8, b'h', b'3'];
            let alpn_ptr = Box::into_raw(Box::new(wire));
            SSL_CTX_set_alpn_select_cb(ctx, Some(alpn_select_cb), alpn_ptr as *mut c_void);

            Ok(Self {
                ctx,
                alpn_wire: alpn_ptr,
            })
        }
    }

    pub fn create_session(&self) -> std::io::Result<TlsSession> {
        unsafe {
            let ssl = SSL_new(self.ctx);
            if ssl.is_null() {
                return Err(std::io::Error::other("SSL_new failed"));
            }
            SSL_set_accept_state(ssl);
            Ok(TlsSession { ssl })
        }
    }
}

impl Drop for TlsContext {
    fn drop(&mut self) {
        unsafe {
            if !self.alpn_wire.is_null() {
                drop(Box::from_raw(self.alpn_wire));
            }
            if !self.ctx.is_null() {
                SSL_CTX_free(self.ctx);
            }
        }
    }
}

/// 接続ごとの SSL セッション
pub struct TlsSession {
    ssl: *mut SSL,
}

impl TlsSession {
    pub fn as_void_ptr(&self) -> *mut c_void {
        self.ssl as *mut c_void
    }

    /// ngtcp2_crypto_conn_ref を SSL に紐付ける
    pub fn attach_conn_ref(&mut self, conn_ref: &mut ConnRef) {
        unsafe {
            SSL_set_ex_data(
                self.ssl,
                0,
                &mut conn_ref.inner as *mut ngtcp2_crypto_conn_ref as *mut c_void,
            );
        }
    }

    pub fn set_quic_transport_params(&mut self, params: &[u8]) -> std::io::Result<()> {
        let rv = unsafe {
            aws_lc_sys::SSL_set_quic_transport_params(self.ssl, params.as_ptr(), params.len())
        };
        if rv != 1 {
            return Err(std::io::Error::other(
                "SSL_set_quic_transport_params failed",
            ));
        }
        Ok(())
    }
}

impl Drop for TlsSession {
    fn drop(&mut self) {
        if !self.ssl.is_null() {
            unsafe { SSL_free(self.ssl) };
        }
    }
}

unsafe extern "C" fn alpn_select_cb(
    _ssl: *mut SSL,
    out: *mut *const u8,
    outlen: *mut u8,
    _in: *const u8,
    _inlen: u32,
    arg: *mut c_void,
) -> i32 {
    // 常に h3 を選択
    let wire = &*(arg as *const Vec<u8>);
    if wire.len() >= 3 {
        *out = wire.as_ptr().add(1);
        *outlen = wire[0];
        return aws_lc_sys::SSL_TLSEXT_ERR_OK as i32;
    }
    aws_lc_sys::SSL_TLSEXT_ERR_NOACK as i32
}
