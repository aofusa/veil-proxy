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
