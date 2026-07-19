//! ngtcp2 QUIC 接続ラッパ（サーバ）

use std::collections::VecDeque;
use std::ffi::c_void;
use std::net::SocketAddr;
use std::os::raw::c_int;
use std::ptr;

use ngtcp2_sys::*;

use super::crypto::TlsSession;

/// QUIC v1
pub const QUIC_V1: u32 = 0x0000_0001;

/// 受信ストリーム断片
pub struct StreamData {
    pub stream_id: i64,
    pub data: Vec<u8>,
    pub fin: bool,
}

pub struct ConnRef {
    pub inner: ngtcp2_crypto_conn_ref,
}

struct ConnUserData {
    stream_queue: VecDeque<StreamData>,
    /// get_new_connection_id で発行した SCID（ConnMap エイリアス登録用）
    new_scids: VecDeque<Vec<u8>>,
    /// 退役した SCID
    retired_scids: VecDeque<Vec<u8>>,
}

/// サーバ側 QUIC 接続
pub struct QuicConn {
    inner: *mut ngtcp2_conn,
    user_data: Box<ConnUserData>,
    _tls: TlsSession,
    _conn_ref: Box<ConnRef>,
    /// path 書き込み用スクラッチ（write_pkt が上書き）
    local_ss: libc::sockaddr_storage,
    remote_ss: libc::sockaddr_storage,
}

impl QuicConn {
    /// 新規サーバ接続を作成
    ///
    /// * `client_scid` — Initial の SCID（サーバから見た DCID）
    /// * `server_scid` — サーバが選ぶ SCID
    /// * `original_dcid` — Initial の DCID（transport params 必須）
    #[allow(clippy::too_many_arguments)]
    pub fn server_new(
        client_scid: &[u8],
        server_scid: &[u8],
        original_dcid: &[u8],
        local: SocketAddr,
        remote: SocketAddr,
        version: u32,
        mut tls: TlsSession,
        max_idle_ms: u64,
        max_udp_payload: u64,
        initial_max_data: u64,
        initial_max_stream_bidi: u64,
        initial_max_stream_uni: u64,
        initial_max_streams_bidi: u64,
        initial_max_streams_uni: u64,
        ts: u64,
    ) -> std::io::Result<Self> {
        let callbacks = server_callbacks();

        let mut settings: ngtcp2_settings = unsafe { std::mem::zeroed() };
        unsafe {
            ngtcp2_settings_default_versioned(NGTCP2_SETTINGS_VERSION as c_int, &mut settings);
        }
        settings.initial_ts = ts;
        settings.max_tx_udp_payload_size = max_udp_payload as usize;

        let mut params: ngtcp2_transport_params = unsafe { std::mem::zeroed() };
        unsafe {
            ngtcp2_transport_params_default_versioned(
                NGTCP2_TRANSPORT_PARAMS_VERSION as c_int,
                &mut params,
            );
        }
        params.initial_max_data = initial_max_data;
        params.initial_max_stream_data_bidi_local = initial_max_stream_bidi;
        params.initial_max_stream_data_bidi_remote = initial_max_stream_bidi;
        params.initial_max_stream_data_uni = initial_max_stream_uni;
        params.initial_max_streams_bidi = initial_max_streams_bidi;
        params.initial_max_streams_uni = initial_max_streams_uni;
        // idle timeout: nanoseconds
        params.max_idle_timeout = max_idle_ms.saturating_mul(1_000_000);
        params.active_connection_id_limit = 4;
        // サーバは Initial の DCID を original_dcid として必須設定
        // （ngtcp2 assert: server && params->original_dcid_present）
        params.original_dcid = cid_from_slice(original_dcid);
        params.original_dcid_present = 1;

        let mut user_data = Box::new(ConnUserData {
            stream_queue: VecDeque::new(),
            new_scids: VecDeque::new(),
            retired_scids: VecDeque::new(),
        });
        let user_ptr = &mut *user_data as *mut ConnUserData as *mut c_void;

        let dcid_raw = cid_from_slice(client_scid);
        let scid_raw = cid_from_slice(server_scid);

        let (local_ss, local_len) = sockaddr_to_raw(&local);
        let (remote_ss, remote_len) = sockaddr_to_raw(&remote);

        let path = ngtcp2_path {
            local: ngtcp2_addr {
                addr: &local_ss as *const _ as *mut _,
                addrlen: local_len,
            },
            remote: ngtcp2_addr {
                addr: &remote_ss as *const _ as *mut _,
                addrlen: remote_len,
            },
            user_data: ptr::null_mut(),
        };

        let mut conn: *mut ngtcp2_conn = ptr::null_mut();
        let rv = unsafe {
            ngtcp2_conn_server_new_versioned(
                &mut conn,
                &dcid_raw,
                &scid_raw,
                &path,
                version,
                NGTCP2_CALLBACKS_VERSION as c_int,
                &callbacks,
                NGTCP2_SETTINGS_VERSION as c_int,
                &settings,
                NGTCP2_TRANSPORT_PARAMS_VERSION as c_int,
                &params,
                ptr::null(),
                user_ptr,
            )
        };
        if rv != 0 {
            return Err(std::io::Error::other(format!(
                "ngtcp2_conn_server_new: {rv}"
            )));
        }

        let mut conn_ref = Box::new(ConnRef {
            inner: ngtcp2_crypto_conn_ref {
                get_conn: Some(conn_ref_get_conn),
                user_data: conn as *mut c_void,
            },
        });
        tls.attach_conn_ref(&mut conn_ref);

        unsafe {
            ngtcp2_conn_set_tls_native_handle(conn, tls.as_void_ptr());
        }

        // ローカル TP を TLS に事前設定（aws-lc が ClientHello で peer TP を捨てないように）
        let mut tp_buf = [0u8; 512];
        let tp_len = unsafe {
            ngtcp2_conn_encode_local_transport_params(conn, tp_buf.as_mut_ptr(), tp_buf.len())
        };
        if tp_len < 0 {
            unsafe { ngtcp2_conn_del(conn) };
            return Err(std::io::Error::other("encode local transport params"));
        }
        if let Err(e) = tls.set_quic_transport_params(&tp_buf[..tp_len as usize]) {
            unsafe { ngtcp2_conn_del(conn) };
            return Err(e);
        }

        Ok(Self {
            inner: conn,
            user_data,
            _tls: tls,
            _conn_ref: conn_ref,
            local_ss: unsafe { std::mem::zeroed() },
            remote_ss: unsafe { std::mem::zeroed() },
        })
    }

    pub fn read_pkt(
        &mut self,
        local: SocketAddr,
        remote: SocketAddr,
        data: &[u8],
        ts: u64,
    ) -> std::io::Result<()> {
        let (local_ss, local_len) = sockaddr_to_raw(&local);
        let (remote_ss, remote_len) = sockaddr_to_raw(&remote);
        let path = ngtcp2_path {
            local: ngtcp2_addr {
                addr: &local_ss as *const _ as *mut _,
                addrlen: local_len,
            },
            remote: ngtcp2_addr {
                addr: &remote_ss as *const _ as *mut _,
                addrlen: remote_len,
            },
            user_data: ptr::null_mut(),
        };
        let pi = ngtcp2_pkt_info { ecn: 0 };
        let rv = unsafe {
            ngtcp2_conn_read_pkt_versioned(
                self.inner,
                &path,
                NGTCP2_PKT_INFO_VERSION as c_int,
                &pi,
                data.as_ptr(),
                data.len(),
                ts,
            )
        };
        if rv != 0 && rv != NGTCP2_ERR_CRYPTO {
            // CRYPTO はハンドシェイク中に起き得る — 呼び出し側で判定
            if rv == NGTCP2_ERR_DRAINING || rv == NGTCP2_ERR_CLOSING {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionAborted,
                    format!("ngtcp2 read_pkt {rv}"),
                ));
            }
            if rv == NGTCP2_ERR_DROP_CONN || rv == NGTCP2_ERR_RETRY {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionRefused,
                    format!("ngtcp2 read_pkt {rv}"),
                ));
            }
            if rv != 0 {
                return Err(std::io::Error::other(format!("ngtcp2 read_pkt {rv}")));
            }
        }
        Ok(())
    }

    /// 送信パケットを `buf` に書き出す。戻り値は書き込みバイト数（0 = なし）
    pub fn write_pkt(&mut self, buf: &mut [u8], ts: u64) -> std::io::Result<usize> {
        let mut pi = ngtcp2_pkt_info { ecn: 0 };
        let mut path = ngtcp2_path {
            local: ngtcp2_addr {
                addr: &mut self.local_ss as *mut _ as *mut _,
                addrlen: std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t,
            },
            remote: ngtcp2_addr {
                addr: &mut self.remote_ss as *mut _ as *mut _,
                addrlen: std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t,
            },
            user_data: ptr::null_mut(),
        };
        let rv = unsafe {
            ngtcp2_conn_write_pkt_versioned(
                self.inner,
                &mut path,
                NGTCP2_PKT_INFO_VERSION as c_int,
                &mut pi,
                buf.as_mut_ptr(),
                buf.len(),
                ts,
            )
        };
        if rv < 0 {
            if rv == NGTCP2_ERR_WRITE_MORE as isize || rv == NGTCP2_ERR_NOBUF as isize {
                return Ok(0);
            }
            return Err(std::io::Error::other(format!("ngtcp2 write_pkt {rv}")));
        }
        Ok(rv as usize)
    }

    /// ストリームデータをパケットに載せる
    pub fn write_stream(
        &mut self,
        buf: &mut [u8],
        stream_id: i64,
        data: &[u8],
        fin: bool,
        ts: u64,
    ) -> std::io::Result<(usize, usize)> {
        let mut pi = ngtcp2_pkt_info { ecn: 0 };
        let mut path = ngtcp2_path {
            local: ngtcp2_addr {
                addr: &mut self.local_ss as *mut _ as *mut _,
                addrlen: std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t,
            },
            remote: ngtcp2_addr {
                addr: &mut self.remote_ss as *mut _ as *mut _,
                addrlen: std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t,
            },
            user_data: ptr::null_mut(),
        };
        let mut pdatalen: ngtcp2_ssize = -1;
        // FIN 時は FIN のみ。非 fin でも MORE は付けない（パケットを都度確定させ、
        // 部分書き込みでフレームが欠けるのを防ぐ）。連続書き込みは呼び出し側ループで行う。
        let flags = if fin {
            NGTCP2_WRITE_STREAM_FLAG_FIN
        } else {
            NGTCP2_WRITE_STREAM_FLAG_NONE
        };
        let rv = unsafe {
            ngtcp2_conn_writev_stream_versioned(
                self.inner,
                &mut path,
                NGTCP2_PKT_INFO_VERSION as c_int,
                &mut pi,
                buf.as_mut_ptr(),
                buf.len(),
                &mut pdatalen,
                flags,
                stream_id,
                &ngtcp2_vec {
                    base: data.as_ptr() as *mut u8,
                    len: data.len(),
                },
                1,
                ts,
            )
        };
        if rv < 0 {
            if rv == NGTCP2_ERR_STREAM_DATA_BLOCKED as isize
                || rv == NGTCP2_ERR_STREAM_SHUT_WR as isize
            {
                return Ok((0, 0));
            }
            if rv == NGTCP2_ERR_WRITE_MORE as isize {
                let accepted = if pdatalen >= 0 { pdatalen as usize } else { 0 };
                return Ok((0, accepted));
            }
            return Err(std::io::Error::other(format!("writev_stream {rv}")));
        }
        let accepted = if pdatalen >= 0 { pdatalen as usize } else { 0 };
        Ok((rv as usize, accepted))
    }

    pub fn open_uni_stream(&mut self) -> std::io::Result<i64> {
        let mut sid: i64 = 0;
        let rv = unsafe { ngtcp2_conn_open_uni_stream(self.inner, &mut sid, ptr::null_mut()) };
        if rv != 0 {
            return Err(std::io::Error::other(format!("open_uni_stream {rv}")));
        }
        Ok(sid)
    }

    pub fn is_handshake_completed(&self) -> bool {
        unsafe { ngtcp2_conn_get_handshake_completed(self.inner) != 0 }
    }

    pub fn get_expiry(&self) -> u64 {
        unsafe { ngtcp2_conn_get_expiry(self.inner) }
    }

    pub fn handle_expiry(&mut self, ts: u64) -> std::io::Result<()> {
        let rv = unsafe { ngtcp2_conn_handle_expiry(self.inner, ts) };
        if rv != 0 && rv != NGTCP2_ERR_IDLE_CLOSE {
            return Err(std::io::Error::other(format!("handle_expiry {rv}")));
        }
        Ok(())
    }

    pub fn is_draining(&self) -> bool {
        unsafe { ngtcp2_conn_in_draining_period(self.inner) != 0 }
    }

    pub fn is_closing(&self) -> bool {
        unsafe { ngtcp2_conn_in_closing_period(self.inner) != 0 }
    }

    pub fn poll_stream_data(&mut self) -> Option<StreamData> {
        self.user_data.stream_queue.pop_front()
    }

    /// 新規発行 SCID を drain（ConnMap エイリアス登録用）
    pub fn poll_new_scid(&mut self) -> Option<Vec<u8>> {
        self.user_data.new_scids.pop_front()
    }

    /// 退役 SCID を drain（ConnMap から除去用）
    pub fn poll_retired_scid(&mut self) -> Option<Vec<u8>> {
        self.user_data.retired_scids.pop_front()
    }

    pub fn extend_max_stream_offset(&mut self, stream_id: i64, datalen: u64) {
        unsafe {
            let _ = ngtcp2_conn_extend_max_stream_offset(self.inner, stream_id, datalen);
            ngtcp2_conn_extend_max_offset(self.inner, datalen);
        }
    }
}

impl Drop for QuicConn {
    fn drop(&mut self) {
        if !self.inner.is_null() {
            unsafe { ngtcp2_conn_del(self.inner) };
        }
    }
}

fn cid_from_slice(data: &[u8]) -> ngtcp2_cid {
    let mut cid: ngtcp2_cid = unsafe { std::mem::zeroed() };
    let n = data.len().min(NGTCP2_MAX_CIDLEN as usize);
    cid.datalen = n;
    cid.data[..n].copy_from_slice(&data[..n]);
    cid
}

fn sockaddr_to_raw(addr: &SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    match addr {
        SocketAddr::V4(v4) => {
            let sin = unsafe { &mut *(&mut storage as *mut _ as *mut libc::sockaddr_in) };
            sin.sin_family = libc::AF_INET as libc::sa_family_t;
            sin.sin_port = v4.port().to_be();
            sin.sin_addr.s_addr = u32::from_ne_bytes(v4.ip().octets());
            (
                storage,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        }
        SocketAddr::V6(v6) => {
            let sin6 = unsafe { &mut *(&mut storage as *mut _ as *mut libc::sockaddr_in6) };
            sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            sin6.sin6_port = v6.port().to_be();
            sin6.sin6_addr.s6_addr = v6.ip().octets();
            sin6.sin6_flowinfo = v6.flowinfo();
            sin6.sin6_scope_id = v6.scope_id();
            (
                storage,
                std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
            )
        }
    }
}

fn server_callbacks() -> ngtcp2_callbacks {
    let mut cb: ngtcp2_callbacks = unsafe { std::mem::zeroed() };
    cb.recv_client_initial = Some(ngtcp2_crypto_recv_client_initial_cb);
    cb.recv_crypto_data = Some(ngtcp2_crypto_recv_crypto_data_cb);
    cb.encrypt = Some(ngtcp2_crypto_encrypt_cb);
    cb.decrypt = Some(ngtcp2_crypto_decrypt_cb);
    cb.hp_mask = Some(ngtcp2_crypto_hp_mask_cb);
    cb.update_key = Some(ngtcp2_crypto_update_key_cb);
    cb.delete_crypto_aead_ctx = Some(ngtcp2_crypto_delete_crypto_aead_ctx_cb);
    cb.delete_crypto_cipher_ctx = Some(ngtcp2_crypto_delete_crypto_cipher_ctx_cb);
    cb.get_path_challenge_data = Some(ngtcp2_crypto_get_path_challenge_data_cb);
    cb.version_negotiation = Some(ngtcp2_crypto_version_negotiation_cb);
    cb.recv_stream_data = Some(recv_stream_data_cb);
    cb.rand = Some(rand_cb);
    cb.get_new_connection_id = Some(get_new_connection_id_cb);
    cb.remove_connection_id = Some(remove_connection_id_cb);
    cb
}

unsafe extern "C" fn conn_ref_get_conn(conn_ref: *mut ngtcp2_crypto_conn_ref) -> *mut ngtcp2_conn {
    (*conn_ref).user_data as *mut ngtcp2_conn
}

unsafe extern "C" fn rand_cb(buf: *mut u8, buflen: usize, _ctx: *const ngtcp2_rand_ctx) {
    let slice = std::slice::from_raw_parts_mut(buf, buflen);
    let _ = aws_lc_rs::rand::fill(slice);
}

unsafe extern "C" fn get_new_connection_id_cb(
    _conn: *mut ngtcp2_conn,
    cid: *mut ngtcp2_cid,
    token: *mut u8,
    cidlen: usize,
    user_data: *mut c_void,
) -> c_int {
    let cid_slice = std::slice::from_raw_parts_mut((*cid).data.as_mut_ptr(), cidlen);
    if aws_lc_rs::rand::fill(cid_slice).is_err() {
        return NGTCP2_ERR_CALLBACK_FAILURE;
    }
    (*cid).datalen = cidlen;
    let token_slice =
        std::slice::from_raw_parts_mut(token, NGTCP2_STATELESS_RESET_TOKENLEN as usize);
    if aws_lc_rs::rand::fill(token_slice).is_err() {
        return NGTCP2_ERR_CALLBACK_FAILURE;
    }
    if !user_data.is_null() {
        let ud = &mut *(user_data as *mut ConnUserData);
        ud.new_scids.push_back(cid_slice[..cidlen].to_vec());
    }
    0
}

unsafe extern "C" fn remove_connection_id_cb(
    _conn: *mut ngtcp2_conn,
    cid: *const ngtcp2_cid,
    user_data: *mut c_void,
) -> c_int {
    if user_data.is_null() || cid.is_null() {
        return 0;
    }
    let ud = &mut *(user_data as *mut ConnUserData);
    let len = (*cid).datalen;
    let data_ptr = (*cid).data.as_ptr();
    let bytes = std::slice::from_raw_parts(data_ptr, len).to_vec();
    ud.retired_scids.push_back(bytes);
    0
}

unsafe extern "C" fn recv_stream_data_cb(
    _conn: *mut ngtcp2_conn,
    flags: u32,
    stream_id: i64,
    _offset: u64,
    data: *const u8,
    datalen: usize,
    user_data: *mut c_void,
    _stream_user_data: *mut c_void,
) -> c_int {
    if user_data.is_null() {
        return 0;
    }
    let ud = &mut *(user_data as *mut ConnUserData);
    let bytes = if datalen > 0 && !data.is_null() {
        std::slice::from_raw_parts(data, datalen).to_vec()
    } else {
        Vec::new()
    };
    let fin = (flags & NGTCP2_STREAM_DATA_FLAG_FIN) != 0;
    ud.stream_queue.push_back(StreamData {
        stream_id,
        data: bytes,
        fin,
    });
    0
}

/// 初期パケットの Destination CID 等をデコード
pub fn accept_packet(pkt: &[u8]) -> Option<(u32, Vec<u8>, Vec<u8>)> {
    let mut hd: ngtcp2_pkt_hd = unsafe { std::mem::zeroed() };
    let rv = unsafe { ngtcp2_accept(&mut hd, pkt.as_ptr(), pkt.len()) };
    if rv != 0 {
        return None;
    }
    let dcid = hd.dcid.data[..hd.dcid.datalen].to_vec();
    let scid = hd.scid.data[..hd.scid.datalen].to_vec();
    Some((hd.version, dcid, scid))
}

/// サーバが使う SCID 長（新規接続時に生成する長さと一致させること）
pub const SERVER_SCID_LEN: usize = 16;

/// 任意パケットから DCID を取り出す（接続ルックアップ用）
///
/// Short header は DCID 長をワイヤに持たないため、サーバ SCID 長を既知として渡す。
pub fn extract_dcid(pkt: &[u8]) -> Option<Vec<u8>> {
    if pkt.is_empty() {
        return None;
    }
    let mut hd: ngtcp2_pkt_hd = unsafe { std::mem::zeroed() };
    // long header を先に試す（Initial 等）
    let rv = unsafe { ngtcp2_pkt_decode_hd_long(&mut hd, pkt.as_ptr(), pkt.len()) };
    if rv >= 0 {
        return Some(hd.dcid.data[..hd.dcid.datalen].to_vec());
    }
    // short header: サーバ SCID 長で DCID を切る
    let rv =
        unsafe { ngtcp2_pkt_decode_hd_short(&mut hd, pkt.as_ptr(), pkt.len(), SERVER_SCID_LEN) };
    if rv >= 0 {
        return Some(hd.dcid.data[..hd.dcid.datalen].to_vec());
    }
    None
}

pub fn timestamp_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}
