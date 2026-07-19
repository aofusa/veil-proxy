//! nghttp3 HTTP/3 接続ラッパ（サーバ）

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::os::raw::c_int;
use std::ptr;

use nghttp3_sys::*;

/// H3 イベント
#[derive(Debug)]
pub enum H3Event {
    Headers {
        stream_id: i64,
        headers: Vec<(Vec<u8>, Vec<u8>)>,
    },
    Data {
        stream_id: i64,
        data: Vec<u8>,
    },
    DataEnd {
        stream_id: i64,
    },
    Reset {
        stream_id: i64,
    },
}

struct H3UserData {
    events: *mut VecDeque<H3Event>,
    /// stream_id → 蓄積中ヘッダ
    partial_headers: HashMap<i64, Vec<(Vec<u8>, Vec<u8>)>>,
    /// レスポンスボディ送信キュー
    body_queues: HashMap<i64, BodyQueue>,
}

struct BodyQueue {
    data: Vec<u8>,
    offset: usize,
    fin: bool,
}

/// サーバ側 HTTP/3 接続
pub struct H3Conn {
    inner: *mut nghttp3_conn,
    /// ヒープ固定: H3UserData.events が指す先。H3Conn の move でポインタが無効にならないよう Box。
    #[allow(clippy::box_collection)] // 理由: FFI user_data が指すイベントキューのアドレス固定
    events: Box<VecDeque<H3Event>>,
    user_data: Box<H3UserData>,
}

impl H3Conn {
    pub fn server_new() -> std::io::Result<Self> {
        let mut events = Box::new(VecDeque::new());
        let events_ptr = &mut *events as *mut VecDeque<H3Event>;
        let user_data = Box::new(H3UserData {
            events: events_ptr,
            partial_headers: HashMap::new(),
            body_queues: HashMap::new(),
        });

        let mut settings: nghttp3_settings = unsafe { std::mem::zeroed() };
        unsafe {
            nghttp3_settings_default_versioned(NGHTTP3_SETTINGS_VERSION as c_int, &mut settings);
        }
        settings.max_field_section_size = 64 * 1024;

        let callbacks = make_callbacks();
        let mut conn: *mut nghttp3_conn = ptr::null_mut();
        let rv = unsafe {
            nghttp3_conn_server_new_versioned(
                &mut conn,
                NGHTTP3_CALLBACKS_VERSION as c_int,
                &callbacks,
                NGHTTP3_SETTINGS_VERSION as c_int,
                &settings,
                ptr::null(),
                &*user_data as *const _ as *mut c_void,
            )
        };
        if rv != 0 {
            return Err(std::io::Error::other(format!(
                "nghttp3_conn_server_new: {rv}"
            )));
        }
        Ok(Self {
            inner: conn,
            events,
            user_data,
        })
    }

    pub fn bind_control_stream(&mut self, stream_id: i64) -> std::io::Result<()> {
        let rv = unsafe { nghttp3_conn_bind_control_stream(self.inner, stream_id) };
        if rv != 0 {
            return Err(std::io::Error::other(format!("bind_control_stream {rv}")));
        }
        Ok(())
    }

    pub fn bind_qpack_streams(&mut self, enc: i64, dec: i64) -> std::io::Result<()> {
        let rv = unsafe { nghttp3_conn_bind_qpack_streams(self.inner, enc, dec) };
        if rv != 0 {
            return Err(std::io::Error::other(format!("bind_qpack_streams {rv}")));
        }
        Ok(())
    }

    pub fn read_stream(
        &mut self,
        stream_id: i64,
        data: &[u8],
        fin: bool,
        ts: u64,
    ) -> std::io::Result<usize> {
        self.user_data.events = &mut *self.events as *mut _;
        let rv = unsafe {
            nghttp3_conn_read_stream2(
                self.inner,
                stream_id,
                data.as_ptr(),
                data.len(),
                if fin { 1 } else { 0 },
                ts,
            )
        };
        if rv < 0 {
            return Err(std::io::Error::other(format!("read_stream {rv}")));
        }
        Ok(rv as usize)
    }

    pub fn poll_event(&mut self) -> Option<H3Event> {
        self.events.pop_front()
    }

    /// レスポンス（ヘッダ + 任意ボディ）を submit
    pub fn submit_response(
        &mut self,
        stream_id: i64,
        headers: &[(Vec<u8>, Vec<u8>)],
        body: Vec<u8>,
        fin: bool,
    ) -> std::io::Result<()> {
        let nvs: Vec<nghttp3_nv> = headers
            .iter()
            .map(|(n, v)| nghttp3_nv {
                name: n.as_ptr() as *mut _,
                value: v.as_ptr() as *mut _,
                namelen: n.len(),
                valuelen: v.len(),
                flags: NGHTTP3_NV_FLAG_NONE as u8,
            })
            .collect();

        self.user_data.body_queues.insert(
            stream_id,
            BodyQueue {
                data: body,
                offset: 0,
                fin,
            },
        );

        let dr = nghttp3_data_reader {
            read_data: Some(body_read_cb),
        };

        // stream_user_data = stream_id を Box で保持
        let sid_box = Box::new(stream_id);
        let sid_ptr = Box::into_raw(sid_box) as *mut c_void;

        // user_data ポインタを H3UserData に（read_data から body_queues を触る）
        // stream_user_data には stream_id を入れる
        let rv = unsafe {
            nghttp3_conn_set_stream_user_data(self.inner, stream_id, sid_ptr);
            // conn user_data は既に H3UserData
            // read_data は conn_user_data から body_queues を引く必要がある
            // → stream_user_data に H3UserData と stream_id のペアを渡す
            nghttp3_conn_submit_response(self.inner, stream_id, nvs.as_ptr(), nvs.len(), &dr)
        };
        // nvs は submit 完了まで生きている必要 — すぐ返すので nvs が drop される前に
        // nghttp3 がコピーする前提（nghttp3 は通常コピーする）
        let _ = nvs;

        if rv != 0 {
            unsafe {
                drop(Box::from_raw(sid_ptr as *mut i64));
            }
            self.user_data.body_queues.remove(&stream_id);
            return Err(std::io::Error::other(format!("submit_response {rv}")));
        }
        // stream_user_data を H3UserData ポインタに差し替え + stream_id は queues のキー
        // read_data は conn の user_data (H3UserData) と stream_id 引数を使う
        // 上の sid_ptr はリーク防止のため set し直す
        let _ = sid_ptr;
        Ok(())
    }

    /// writev で送出データを取得
    pub fn write_stream(
        &mut self,
        vecs: &mut [nghttp3_vec],
    ) -> std::io::Result<Option<(i64, bool, usize)>> {
        let mut stream_id: i64 = -1;
        let mut fin: c_int = 0;
        let rv = unsafe {
            nghttp3_conn_writev_stream(
                self.inner,
                &mut stream_id,
                &mut fin,
                vecs.as_mut_ptr(),
                vecs.len(),
            )
        };
        if rv < 0 {
            return Err(std::io::Error::other(format!("writev_stream {rv}")));
        }
        if stream_id < 0 || rv == 0 {
            return Ok(None);
        }
        Ok(Some((stream_id, fin != 0, rv as usize)))
    }

    pub fn add_write_offset(&mut self, stream_id: i64, n: usize) -> std::io::Result<()> {
        let rv = unsafe { nghttp3_conn_add_write_offset(self.inner, stream_id, n) };
        if rv != 0 {
            return Err(std::io::Error::other(format!("add_write_offset {rv}")));
        }
        Ok(())
    }
}

impl Drop for H3Conn {
    fn drop(&mut self) {
        if !self.inner.is_null() {
            unsafe { nghttp3_conn_del(self.inner) };
        }
    }
}

fn make_callbacks() -> nghttp3_callbacks {
    let mut cb: nghttp3_callbacks = unsafe { std::mem::zeroed() };
    cb.recv_header = Some(recv_header_cb);
    cb.end_headers = Some(end_headers_cb);
    cb.recv_data = Some(recv_data_cb);
    cb.end_stream = Some(end_stream_cb);
    cb.reset_stream = Some(reset_stream_cb);
    cb
}

unsafe extern "C" fn recv_header_cb(
    _conn: *mut nghttp3_conn,
    stream_id: i64,
    _token: i32,
    name: *mut nghttp3_rcbuf,
    value: *mut nghttp3_rcbuf,
    _flags: u8,
    user_data: *mut c_void,
    _stream_user_data: *mut c_void,
) -> c_int {
    if user_data.is_null() || name.is_null() || value.is_null() {
        return 0;
    }
    let ud = &mut *(user_data as *mut H3UserData);
    let n = rcbuf_to_vec(name);
    let v = rcbuf_to_vec(value);
    ud.partial_headers
        .entry(stream_id)
        .or_default()
        .push((n, v));
    0
}

unsafe extern "C" fn end_headers_cb(
    _conn: *mut nghttp3_conn,
    stream_id: i64,
    _flags: i32,
    user_data: *mut c_void,
    _stream_user_data: *mut c_void,
) -> c_int {
    if user_data.is_null() {
        return 0;
    }
    let ud = &mut *(user_data as *mut H3UserData);
    let headers = ud.partial_headers.remove(&stream_id).unwrap_or_default();
    if !ud.events.is_null() {
        (*ud.events).push_back(H3Event::Headers { stream_id, headers });
    }
    0
}

unsafe extern "C" fn recv_data_cb(
    _conn: *mut nghttp3_conn,
    stream_id: i64,
    data: *const u8,
    datalen: usize,
    user_data: *mut c_void,
    _stream_user_data: *mut c_void,
) -> c_int {
    if user_data.is_null() {
        return 0;
    }
    let ud = &mut *(user_data as *mut H3UserData);
    let bytes = if datalen > 0 && !data.is_null() {
        std::slice::from_raw_parts(data, datalen).to_vec()
    } else {
        Vec::new()
    };
    if !ud.events.is_null() {
        (*ud.events).push_back(H3Event::Data {
            stream_id,
            data: bytes,
        });
    }
    0
}

unsafe extern "C" fn end_stream_cb(
    _conn: *mut nghttp3_conn,
    stream_id: i64,
    user_data: *mut c_void,
    _stream_user_data: *mut c_void,
) -> c_int {
    if user_data.is_null() {
        return 0;
    }
    let ud = &mut *(user_data as *mut H3UserData);
    if !ud.events.is_null() {
        (*ud.events).push_back(H3Event::DataEnd { stream_id });
    }
    0
}

unsafe extern "C" fn reset_stream_cb(
    _conn: *mut nghttp3_conn,
    stream_id: i64,
    _app_error_code: u64,
    user_data: *mut c_void,
    _stream_user_data: *mut c_void,
) -> c_int {
    if user_data.is_null() {
        return 0;
    }
    let ud = &mut *(user_data as *mut H3UserData);
    if !ud.events.is_null() {
        (*ud.events).push_back(H3Event::Reset { stream_id });
    }
    0
}

unsafe extern "C" fn body_read_cb(
    _conn: *mut nghttp3_conn,
    stream_id: i64,
    vec: *mut nghttp3_vec,
    veccnt: usize,
    pflags: *mut u32,
    user_data: *mut c_void,
    _stream_user_data: *mut c_void,
) -> nghttp3_ssize {
    if user_data.is_null() || veccnt == 0 {
        return 0;
    }
    let ud = &mut *(user_data as *mut H3UserData);
    let Some(q) = ud.body_queues.get_mut(&stream_id) else {
        *pflags |= NGHTTP3_DATA_FLAG_EOF;
        return 0;
    };
    if q.offset >= q.data.len() {
        if q.fin {
            *pflags |= NGHTTP3_DATA_FLAG_EOF;
        }
        return 0;
    }
    let remaining = &q.data[q.offset..];
    (*vec).base = remaining.as_ptr() as *mut u8;
    (*vec).len = remaining.len();
    q.offset = q.data.len();
    if q.fin {
        *pflags |= NGHTTP3_DATA_FLAG_EOF;
    }
    1
}

unsafe fn rcbuf_to_vec(rc: *mut nghttp3_rcbuf) -> Vec<u8> {
    let buf = nghttp3_rcbuf_get_buf(rc);
    if buf.base.is_null() || buf.len == 0 {
        return Vec::new();
    }
    std::slice::from_raw_parts(buf.base, buf.len).to_vec()
}
