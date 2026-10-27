//! 転送のヘルパー（ジェネリック版・kTLS + splice のゼロコピー転送）
//!
//! F-233 で `src/proxy.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// 転送ヘルパー関数（ジェネリック版）
// ====================

/// 正確なバイト数を転送
pub(super) async fn transfer_exact_bytes<R: AsyncReader, W: AsyncWriter>(
    reader: &mut R,
    writer: &mut W,
    mut remaining: usize,
) -> u64 {
    let mut total = 0u64;

    while remaining > 0 {
        let buf = buf_get();
        let read_result = timeout(READ_TIMEOUT, reader.read_buf(buf)).await;

        let (res, mut returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => return total,
        };

        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                break;
            }
            Ok(n) => n.min(remaining),
            Err(_) => {
                buf_put(returned_buf);
                break;
            }
        };

        // SafeReadBuffer の有効長を設定して書き込み用Vecに変換
        returned_buf.set_valid_len(n);
        let write_buf = returned_buf.into_truncated();

        let write_result = timeout(WRITE_TIMEOUT, writer.write_buf(write_buf)).await;
        match write_result {
            Ok((Ok(_), returned)) => {
                buf_put_vec(returned);
            }
            Ok((Err(_), returned)) => {
                buf_put_vec(returned);
                break;
            }
            Err(_) => break,
        }

        total += n as u64;
        remaining -= n;
    }

    total
}

// ====================
// kTLS + splice(2) によるゼロコピー転送（Linux 固有）
// ====================
//
// kTLS が有効な場合、splice(2) を使用してカーネル空間で直接
// データを転送します。これにより、ボディ転送時にユーザー空間への
// コピーが完全に不要になります。
//
// ## 実装状況
//
// - **ファイル送信（sendfile）**: kTLS有効時にゼロコピー対応 ✅
// - **プロキシ転送（splice）**: kTLS有効時 + Content-Length で対応 ✅
//
// ## プロキシ転送でのsplice使用
//
// libc::read/write を直接使用し、ランタイムの所有権ベース I/O を回避。
// 非同期待機は TcpStream::readable()/writable() を使用。
//
// ### 対応状況
// - Content-Length 転送: splice(2) でゼロコピー ✅
// - Chunked 転送: 通常転送（終端検出が必要なため）
//
// ## splice(2) の転送フロー
//
// [リクエスト] クライアント(kTLS) → splice → バックエンド(TCP)
//   1. ヘッダー: raw_read で読み取り → パース → raw_write で送信
//   2. ボディ: splice(2) でゼロコピー転送
//
// [レスポンス] バックエンド(TCP) → splice → クライアント(kTLS)
//   1. ヘッダー: raw_read で読み取り → パース → raw_write で送信
//   2. ボディ: splice(2) でゼロコピー転送
//
// 注意: splice(2) は少なくとも一方のFDがパイプである必要があります。
// ====================

/// Chunkedボディ転送の結果
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum ChunkedTransferResult {
    /// 転送完了
    Complete,
    /// 転送失敗（I/Oエラー等）
    Failed,
    /// サイズ制限超過（DoS対策）
    SizeLimitExceeded,
    /// フレーミングが RFC 9112 §7.1 に合わない（厳格な解析。要求の密輸の防止）
    Invalid,
}

/// Chunkedボディを転送（ステートマシンベース）
///
/// RFC 7230準拠のChunkedDecoderを使用して、トレーラーの有無に
/// かかわらず正確に終端を検出します。
///
/// DoS対策として、max_body_size を超えた場合は転送を中止します。
///
/// # Arguments
/// * `reader` - 読み取り元ストリーム
/// * `writer` - 書き込み先ストリーム
/// * `initial_body` - 初期ボディデータ（ヘッダー後に既に読み取り済みのデータ）
/// * `max_body_size` - 最大許容ボディサイズ（0の場合は制限なし）
pub(super) async fn transfer_chunked_body<R: AsyncReader, W: AsyncWriter>(
    reader: &mut R,
    writer: &mut W,
    initial_body: &[u8],
    max_body_size: u64,
) -> ChunkedTransferResult {
    // 要求の本文は厳格に読み、終端（0 チャンク + トレーラーの空行）より後ろのバイトを上流へ送らない
    // （緩い解析や終端の後ろの転送は、上流で別の要求になる。F-236 の再走査の F1・F5）。
    // 呼び出し側（h1.rs）は、終端より後ろを次の要求として切り離してから initial_body を渡す。
    let mut decoder = ChunkedDecoder::new_request(max_body_size);

    // 初期ボディが既に終端を含んでいるかチェック
    if !initial_body.is_empty() {
        match decoder.feed(initial_body) {
            ChunkedFeedResult::Complete => return ChunkedTransferResult::Complete,
            ChunkedFeedResult::SizeLimitExceeded => {
                return ChunkedTransferResult::SizeLimitExceeded
            }
            ChunkedFeedResult::Invalid => return ChunkedTransferResult::Invalid,
            ChunkedFeedResult::Continue => {}
        }
    }

    loop {
        let buf = buf_get();
        let read_result = timeout(READ_TIMEOUT, reader.read_buf(buf)).await;

        let (res, mut returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => return ChunkedTransferResult::Failed,
        };

        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                break;
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                return ChunkedTransferResult::Failed;
            }
        };

        // SafeReadBuffer の有効長を設定
        returned_buf.set_valid_len(n);

        // ステートマシンにデータをフィード（型安全なアクセス）
        let (feed_result, consumed) = decoder.feed_counted(returned_buf.as_valid_slice());

        // サイズ制限超過・不正なフレーミング
        match feed_result {
            ChunkedFeedResult::SizeLimitExceeded => {
                buf_put(returned_buf);
                return ChunkedTransferResult::SizeLimitExceeded;
            }
            ChunkedFeedResult::Invalid => {
                buf_put(returned_buf);
                return ChunkedTransferResult::Invalid;
            }
            ChunkedFeedResult::Complete | ChunkedFeedResult::Continue => {}
        }

        // バックエンドに転送（有効データのみを含むVecに変換）。終端より後ろ（同じ読み取りに入った
        // パイプラインの次の要求）は送らずに捨てる（上流で veil の検査を通らない要求にさせない）。
        if consumed < n {
            debug!(
                "[proxy] dropping {} bytes after the chunked terminator",
                n - consumed
            );
            returned_buf.set_valid_len(consumed);
        }
        let write_buf = returned_buf.into_truncated();

        let write_result = timeout(WRITE_TIMEOUT, writer.write_buf(write_buf)).await;
        match write_result {
            Ok((Ok(_), returned)) => {
                buf_put_vec(returned);
            }
            Ok((Err(_), returned)) => {
                buf_put_vec(returned);
                return ChunkedTransferResult::Failed;
            }
            Err(_) => return ChunkedTransferResult::Failed,
        }

        // 終端チェック
        if feed_result == ChunkedFeedResult::Complete {
            return ChunkedTransferResult::Complete;
        }
    }

    ChunkedTransferResult::Failed
}

// ====================
// 上流の応答の本文（B-244・B-245・B-246）
// ====================
//
// 上流接続をプールへ戻せるのは、応答の本文の終端（Content-Length の長さ・chunked の終端）を
// ちょうど読み切ったときだけ。途中で止まった（タイムアウト・EOF・不正）・終端の後ろにまだ
// バイトがある・接続の終わりで区切る応答の接続を戻すと、次の要求（別のクライアントのものかもしれない）
// に前の応答の続きが混ざる（B-244）。

/// 最初の読み込みで受けた本文の扱い（[`response_body_start`]）。
pub(super) struct ResponseBodyStart {
    /// 最初の読み込みのうち、クライアントへ送る本文のバイト数
    pub(super) send: usize,
    /// 本文がもう終わった（残りを読まない）
    pub(super) done: bool,
    /// 本文の終端より後ろにバイトがあった（上流接続は再利用しない）
    pub(super) extra: bool,
    decoder: Option<ChunkedDecoder>,
}

impl ResponseBodyStart {
    /// chunked のデコーダ（最初の読み込みを読ませた状態）を取り出す。chunked 以外では新しいデコーダ
    pub(super) fn into_decoder(self) -> ChunkedDecoder {
        self.decoder.unwrap_or_else(ChunkedDecoder::new_unlimited)
    }
}

/// 上流の応答の本文の転送の結果。
pub(super) struct ResponseBodyOutcome {
    /// [`transfer_response_body_rest`] がクライアントへ書いたバイト数（最初の読み込みの分は含まない）
    pub(super) total: u64,
    /// 本文の区切りどおりに終わった（`UntilClose` は上流が閉じた）
    pub(super) complete: bool,
    /// 終端より後ろにバイトがあった
    pub(super) extra: bool,
}

impl ResponseBodyOutcome {
    /// 上流接続をプールへ戻してよいか（上流が keep-alive を許していることは呼び出し側が見る）。
    #[inline]
    pub(super) fn reusable(&self, kind: ResponseBody) -> bool {
        self.complete && !self.extra && !matches!(kind, ResponseBody::UntilClose)
    }

    /// クライアント接続を閉じて終わりを伝えるべきか（本文が途中で止まった・接続の終わりで区切る応答）。
    #[inline]
    pub(super) fn client_must_close(&self, kind: ResponseBody) -> bool {
        !self.complete || matches!(kind, ResponseBody::UntilClose)
    }
}

/// ヘッダと同じ読み込みに入っていた本文（`initial`）のうち、送る分と終わったかを決める。
pub(super) fn response_body_start(kind: ResponseBody, initial: &[u8]) -> ResponseBodyStart {
    match kind {
        ResponseBody::None => ResponseBodyStart {
            send: 0,
            done: true,
            extra: !initial.is_empty(),
            decoder: None,
        },
        ResponseBody::Length(cl) => ResponseBodyStart {
            send: initial.len().min(cl),
            done: initial.len() >= cl,
            extra: initial.len() > cl,
            decoder: None,
        },
        ResponseBody::Chunked => {
            let mut decoder = ChunkedDecoder::new_unlimited();
            let (res, consumed) = decoder.feed_counted(initial);
            let done = res == ChunkedFeedResult::Complete;
            ResponseBodyStart {
                send: consumed,
                done,
                extra: consumed < initial.len(),
                decoder: Some(decoder),
            }
        }
        ResponseBody::UntilClose => ResponseBodyStart {
            send: initial.len(),
            done: false,
            extra: false,
            decoder: None,
        },
    }
}

/// 本文の残りを区切りどおりに上流からクライアントへ転送する（`start` は [`response_body_start`] の結果。
/// 最初の読み込みの分は呼び出し側がヘッダと一緒に送ってある）。`cache` には送った本文を足し、
/// 区切りどおりに終わったときだけ保存する（途中で止まった応答をキャッシュしない）。
pub(super) async fn transfer_response_body_rest<R: AsyncReader, W: AsyncWriter>(
    backend: &mut R,
    client: &mut W,
    kind: ResponseBody,
    start: ResponseBodyStart,
    mut cache: Option<&mut CacheSaveContext>,
) -> ResponseBodyOutcome {
    let mut out = ResponseBodyOutcome {
        total: 0,
        complete: start.done,
        extra: start.extra,
    };
    let mut decoder = start.decoder;
    let mut remaining = match kind {
        ResponseBody::Length(cl) => cl.saturating_sub(start.send),
        _ => 0,
    };
    while !out.complete {
        let (res, mut buf) = match timeout(READ_TIMEOUT, backend.read_buf(buf_get())).await {
            Ok(r) => r,
            // 上流が止まった: 本文は途中（B-244。以前は chunked で黙って break し、接続をプールへ戻していた）
            Err(_) => break,
        };
        let n = match res {
            Ok(n) if n > 0 => n,
            Ok(_) => {
                buf_put(buf);
                // 接続の終わりで区切る応答だけは EOF が終端。TLS は close_notify を受けたときだけ（F-282:
                // 受けずに切れたら切り詰めかもしれないので不完全として切断で伝える）
                out.complete = matches!(kind, ResponseBody::UntilClose) && backend.eof_was_clean();
                break;
            }
            Err(_) => {
                buf_put(buf);
                break;
            }
        };
        buf.set_valid_len(n);
        let send = match kind {
            ResponseBody::Length(_) => {
                let send = n.min(remaining);
                remaining -= send;
                out.extra |= n > send;
                out.complete = remaining == 0;
                send
            }
            ResponseBody::Chunked => {
                let Some(d) = decoder.as_mut() else {
                    buf_put(buf);
                    break;
                };
                let (fr, consumed) = d.feed_counted(buf.as_valid_slice());
                match fr {
                    ChunkedFeedResult::Complete => {
                        out.complete = true;
                        out.extra |= consumed < n;
                        consumed
                    }
                    ChunkedFeedResult::Continue => consumed,
                    // 上流の chunked が壊れている: ここまでで打ち切る（続きを次の要求の応答にしない）
                    ChunkedFeedResult::Invalid | ChunkedFeedResult::SizeLimitExceeded => {
                        buf_put(buf);
                        break;
                    }
                }
            }
            ResponseBody::UntilClose => n,
            ResponseBody::None => {
                buf_put(buf);
                break;
            }
        };
        if send == 0 {
            buf_put(buf);
            continue;
        }
        buf.set_valid_len(send);
        if let Some(ctx) = cache.as_deref_mut() {
            ctx.append_body(buf.as_valid_slice());
        }
        match timeout(WRITE_TIMEOUT, client.write_buf(buf.into_truncated())).await {
            Ok((Ok(_), returned)) => buf_put_vec(returned),
            Ok((Err(_), returned)) => {
                buf_put_vec(returned);
                out.complete = false;
                break;
            }
            Err(_) => {
                out.complete = false;
                break;
            }
        }
        out.total += send as u64;
    }
    if out.complete && matches!(kind, ResponseBody::Length(_) | ResponseBody::Chunked) {
        if let Some(ctx) = cache {
            ctx.save_to_cache();
        }
    }
    out
}

/// 上流の応答の本文を区切りどおりに最後まで集める（chunked は枠のまま）。区切りどおりに
/// 終わらなければ `None`（途中までを完結した応答に見せない。B-244）。2 つ目は終端の後ろにバイトがあったか。
pub(crate) async fn collect_response_body_raw<R: AsyncReadRent + Unpin>(
    backend: &mut R,
    kind: ResponseBody,
    initial: &[u8],
) -> Option<(Vec<u8>, bool)> {
    let start = response_body_start(kind, initial);
    let mut raw = initial[..start.send].to_vec();
    let mut extra = start.extra;
    let mut complete = start.done;
    let mut decoder = start.decoder;
    let mut remaining = match kind {
        ResponseBody::Length(cl) => cl.saturating_sub(start.send),
        _ => 0,
    };
    while !complete {
        let (res, mut buf) = timeout(READ_TIMEOUT, backend.read(buf_get())).await.ok()?;
        let n = match res {
            Ok(n) if n > 0 => n,
            res => {
                buf_put(buf);
                // 接続の終わりで区切る応答だけは EOF が終端。TLS は close_notify を受けたときだけ（F-282:
                // 受けずに切れたら切り詰めかもしれない）。読み取りの誤りは終端にしない
                if res.is_ok()
                    && matches!(kind, ResponseBody::UntilClose)
                    && backend.eof_was_clean()
                {
                    break;
                }
                return None;
            }
        };
        buf.set_valid_len(n);
        let take = match kind {
            ResponseBody::Length(_) => {
                let take = n.min(remaining);
                remaining -= take;
                extra |= n > take;
                complete = remaining == 0;
                take
            }
            ResponseBody::Chunked => {
                let d = decoder.as_mut()?;
                let (fr, consumed) = d.feed_counted(buf.as_valid_slice());
                match fr {
                    ChunkedFeedResult::Complete => {
                        complete = true;
                        extra |= consumed < n;
                    }
                    ChunkedFeedResult::Continue => {}
                    ChunkedFeedResult::Invalid | ChunkedFeedResult::SizeLimitExceeded => {
                        buf_put(buf);
                        return None;
                    }
                }
                consumed
            }
            ResponseBody::UntilClose => n,
            ResponseBody::None => 0,
        };
        raw.extend_from_slice(&buf.as_valid_slice()[..take]);
        buf_put(buf);
    }
    Some((raw, extra))
}

/// [`collect_response_body_raw`] と同じだが、chunked は復号して返す（圧縮・HTTP/2 への中継のため）。
#[cfg(any(feature = "compression", feature = "http2"))]
pub(super) async fn collect_response_body<R: AsyncReadRent + Unpin>(
    backend: &mut R,
    kind: ResponseBody,
    initial: &[u8],
) -> Option<(Vec<u8>, bool)> {
    let (raw, extra) = collect_response_body_raw(backend, kind, initial).await?;
    if matches!(kind, ResponseBody::Chunked) {
        return Some((decode_chunked_body(&raw), extra));
    }
    Some((raw, extra))
}

#[cfg(test)]
mod eof_tests {
    use super::*;
    use crate::runtime::buf::IoBufMut;

    #[cfg(all(veil_rt_uring, target_os = "linux"))]
    fn runtime_available() -> bool {
        crate::runtime::ring::IoUring::new(8, 0).is_ok()
    }

    #[cfg(not(all(veil_rt_uring, target_os = "linux")))]
    fn runtime_available() -> bool {
        true
    }

    /// 決まった塊を返してから EOF になる上流（`clean` は EOF が close_notify 付きか）
    struct Upstream {
        chunks: Vec<&'static [u8]>,
        clean: bool,
    }

    impl AsyncReader for Upstream {
        async fn read_buf(
            &mut self,
            mut buf: SafeReadBuffer,
        ) -> (io::Result<usize>, SafeReadBuffer) {
            if self.chunks.is_empty() {
                return (Ok(0), buf);
            }
            let c = self.chunks.remove(0);
            let n = c.len().min(buf.bytes_total());
            // SAFETY: write_ptr は bytes_total バイト書ける。n バイトだけ書いて初期化済みにする
            unsafe {
                std::ptr::copy_nonoverlapping(c.as_ptr(), buf.write_ptr(), n);
                buf.set_init(n);
            }
            (Ok(n), buf)
        }

        fn eof_was_clean(&self) -> bool {
            self.clean
        }
    }

    impl crate::runtime::io::AsyncReadRent for Upstream {
        async fn read<T: IoBufMut>(
            &mut self,
            mut buf: T,
        ) -> crate::runtime::io::BufResult<usize, T> {
            if self.chunks.is_empty() {
                return (Ok(0), buf);
            }
            let c = self.chunks.remove(0);
            let n = c.len().min(buf.bytes_total());
            // SAFETY: write_ptr は bytes_total バイト書ける。n バイトだけ書いて初期化済みにする
            unsafe {
                std::ptr::copy_nonoverlapping(c.as_ptr(), buf.write_ptr(), n);
                buf.set_init(n);
            }
            (Ok(n), buf)
        }

        fn eof_was_clean(&self) -> bool {
            self.clean
        }
    }

    struct Sink(Vec<u8>);

    impl AsyncWriter for Sink {
        async fn write_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
            self.0.extend_from_slice(&buf);
            (Ok(buf.len()), buf)
        }
    }

    /// F-282: 接続の終わりで区切る応答は、TLS の close_notify を受けた EOF のときだけ完了とみなす
    /// （受けずに切れた応答は切り詰めかもしれないので不完全。キャッシュせず、再利用もしない）
    #[test]
    fn until_close_body_needs_clean_eof() {
        if !runtime_available() {
            return;
        }
        crate::runtime::block_on(async {
            for (clean, complete) in [(true, true), (false, false)] {
                let mut up = Upstream {
                    chunks: vec![b"hello ", b"world"],
                    clean,
                };
                let mut sink = Sink(Vec::new());
                let start = response_body_start(ResponseBody::UntilClose, b"");
                let out = transfer_response_body_rest(
                    &mut up,
                    &mut sink,
                    ResponseBody::UntilClose,
                    start,
                    None,
                )
                .await;
                assert_eq!(out.complete, complete, "clean EOF = {clean}");
                assert_eq!(sink.0, b"hello world");
                assert!(out.client_must_close(ResponseBody::UntilClose));
            }
        });
    }

    /// F-282: 本文を集める経路（HTTP/2 への中継・圧縮）も、接続の終わりで区切る応答は close_notify を受けた
    /// EOF のときだけ完全な本文として返す（受けずに切れたら None = 502）
    #[cfg(any(feature = "compression", feature = "http2"))]
    #[test]
    fn collected_until_close_body_needs_clean_eof() {
        if !runtime_available() {
            return;
        }
        crate::runtime::block_on(async {
            for clean in [true, false] {
                let mut up = Upstream {
                    chunks: vec![b"world"],
                    clean,
                };
                let got = collect_response_body(&mut up, ResponseBody::UntilClose, b"hello ").await;
                if clean {
                    assert_eq!(got, Some((b"hello world".to_vec(), false)));
                } else {
                    assert_eq!(
                        got, None,
                        "a body cut without close_notify must not be complete"
                    );
                }
            }
        });
    }
}
