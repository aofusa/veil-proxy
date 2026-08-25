//! WASM gRPC 呼び出しの接続プール（F-139）
//!
//! `grpc_executor::GrpcRunner`（専用 gRPC 実行スレッドの状態機械）専用に使われる。
//! このスレッドの内部状態としてのみ保持され、グローバル static としては公開しない
//! （設計書 `docs/artifacts/f139_wasm_grpc_nonblocking_design.md` の「単純化のため
//! 専用スレッド内の HashMap で持ち、グローバル static は使わない」方針）。
//!
//! 「1 接続 1 アクティブストリーム」方式（チェックアウト / チェックイン）。
//! HTTP/2 の per-stream 多重化までは行わない。再利用の主目的である
//! **TCP + TLS ハンドシェイクの排除**はこれで達成できる。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::grpc_tls::ClientStream;
use crate::http2::hpack::{HpackDecoder, HpackEncoder};

/// 接続プールのキー: (host, port, tls)
pub(super) type ConnKey = (String, u16, bool);

/// アイドル接続の最大保持時間。これを超えたらプールから破棄し、次回は新規接続する。
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// クライアント発ストリーム ID の上限（RFC 7540: 31 ビット、最上位ビットは予約）。
/// これに達した接続はプールへ戻さず破棄する。
pub(super) const MAX_STREAM_ID: u32 = (1u32 << 31) - 1;

/// プールされた 1 本の gRPC-over-h2/h2c 接続。
pub(super) struct PooledConn {
    pub(super) stream: ClientStream,
    pub(super) hpack_enc: HpackEncoder,
    pub(super) hpack_dec: HpackDecoder,
    /// 次に割り当てるクライアント発ストリーム ID（奇数から 2 ずつ増加）。
    pub(super) next_stream_id: u32,
    /// 接続ごとに 1 回だけ確保して再利用する受信バッファ。
    pub(super) read_buf: Vec<u8>,
    /// 送信待ちバイト列（部分書き込みの持ち越し用）。
    pub(super) write_buf: Vec<u8>,
    /// `write_buf` のうち、まだ送出できていない先頭オフセット。
    pub(super) write_offset: usize,
    /// クライアントプリフェース + 初期 SETTINGS を送出済みか。
    pub(super) preface_sent: bool,
    /// サーバーから GOAWAY を受け取った（再利用不可）。
    pub(super) goaway: bool,
    pub(super) last_used: Instant,
    /// 接続レベルの送信フロー制御ウィンドウ（初期値 65535、WINDOW_UPDATE で増加）。
    pub(super) conn_send_window: i64,
    /// サーバーが SETTINGS で通知した INITIAL_WINDOW_SIZE（新規ストリームの初期値）。
    pub(super) peer_initial_window: i64,
}

impl PooledConn {
    /// 新規接続（プールミス時）用の初期状態を構築する。
    pub(super) fn new(stream: ClientStream, header_table_size: usize) -> Self {
        Self {
            stream,
            hpack_enc: HpackEncoder::new(header_table_size),
            hpack_dec: HpackDecoder::new(header_table_size),
            next_stream_id: 1,
            read_buf: Vec::with_capacity(8192),
            write_buf: Vec::new(),
            write_offset: 0,
            preface_sent: false,
            goaway: false,
            last_used: Instant::now(),
            conn_send_window: crate::http2::settings::defaults::INITIAL_WINDOW_SIZE as i64,
            peer_initial_window: crate::http2::settings::defaults::INITIAL_WINDOW_SIZE as i64,
        }
    }

    /// この接続がまだ再利用可能か（GOAWAY 未受信、ストリーム ID に余裕がある、
    /// アイドル時間が閾値以内）。
    pub(super) fn is_reusable(&self) -> bool {
        !self.goaway && self.next_stream_id <= MAX_STREAM_ID
    }
}

/// (host, port, tls) ごとの接続プール本体。
#[derive(Default)]
pub(super) struct GrpcConnPool {
    conns: HashMap<ConnKey, Vec<PooledConn>>,
    /// テスト・観測用の統計（受け入れ条件: 「プールが同一キーの 2 回目の呼び出しで
    /// ハンドシェイクを再実行しないこと」をヒット数で検証する）。
    pub(super) hits: usize,
    pub(super) misses: usize,
}

impl GrpcConnPool {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// キーに対応するアイドル接続を 1 本取り出す（あれば）。
    /// アイドルタイムアウト超過・再利用不可の接続は破棄しつつ探す。
    pub(super) fn checkout(&mut self, key: &ConnKey) -> Option<PooledConn> {
        if let Some(list) = self.conns.get_mut(key) {
            while let Some(conn) = list.pop() {
                if conn.is_reusable() && conn.last_used.elapsed() <= IDLE_TIMEOUT {
                    self.hits += 1;
                    return Some(conn);
                }
                // 破棄（GOAWAY 済み・アイドル超過・ストリーム ID 枯渇）。
            }
        }
        self.misses += 1;
        None
    }

    /// 呼び出し完了後、接続をプールへ戻す（再利用可能な場合のみ）。
    pub(super) fn checkin(&mut self, key: ConnKey, mut conn: PooledConn) {
        if !conn.is_reusable() {
            return;
        }
        conn.last_used = Instant::now();
        self.conns.entry(key).or_default().push(conn);
    }

    /// アイドルタイムアウトを超えた接続を一掃する（実行スレッドの待機直前などに呼ぶ）。
    pub(super) fn sweep_idle(&mut self) {
        for list in self.conns.values_mut() {
            list.retain(|c| c.is_reusable() && c.last_used.elapsed() <= IDLE_TIMEOUT);
        }
        self.conns.retain(|_, v| !v.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn dummy_conn() -> PooledConn {
        // 理由付き allow: 単体テスト（コールドパス）専用の同期 TCP。
        // 本体のホットパス（io_uring イベントループ）には無関係。
        #[allow(clippy::disallowed_methods)]
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        #[allow(clippy::disallowed_methods)]
        let stream = std::net::TcpStream::connect(addr).expect("connect should succeed");
        drop(listener);
        PooledConn::new(ClientStream::Plain(stream), 4096)
    }

    /// 同一キーへの 2 回目の checkout はプールにヒットし、
    /// 新規接続（handshake 相当）が発生しないことを検証する。
    #[test]
    fn test_pool_checkout_checkin_hits() {
        let mut pool = GrpcConnPool::new();
        let key: ConnKey = ("backend".to_string(), 8080, false);

        // 1 回目: プールが空なのでミス。
        assert!(pool.checkout(&key).is_none());
        assert_eq!(pool.misses, 1);
        assert_eq!(pool.hits, 0);

        // 呼び出し完了後にチェックイン。
        pool.checkin(key.clone(), dummy_conn());

        // 2 回目: 同一キーでヒットする（新規接続不要）。
        let conn = pool.checkout(&key);
        assert!(conn.is_some());
        assert_eq!(pool.hits, 1);
        assert_eq!(pool.misses, 1);

        // 取り出した後は空になっているので次はまたミス。
        assert!(pool.checkout(&key).is_none());
        assert_eq!(pool.misses, 2);
    }

    /// GOAWAY 済みの接続はチェックインしても再利用されない。
    #[test]
    fn test_pool_discards_goaway_connection() {
        let mut pool = GrpcConnPool::new();
        let key: ConnKey = ("backend".to_string(), 8080, false);
        let mut conn = dummy_conn();
        conn.goaway = true;
        pool.checkin(key.clone(), conn);
        assert!(pool.checkout(&key).is_none());
    }
}
