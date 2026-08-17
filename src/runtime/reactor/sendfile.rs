//! FreeBSD `sendfile(2)` によるゼロコピー静的ファイル送信（F-141、`target_os = "freebsd"` 専用）
//!
//! Linux の `splice(2)` 経路（`reactor::splice`、`target_os = "linux"` 専用）に相当する、
//! ファイル fd → ソケット fd のカーネル内ゼロコピー転送を FreeBSD の `sendfile(2)` で提供する。
//! 両者は完全に別実装であり、互いのコードパスには一切影響しない。
//!
//! # `SF_NODISKIO` を必須にする理由（ホットパス絶対規則）
//!
//! FreeBSD の `sendfile(2)` は、対象ソケットが非ブロッキングであっても **ファイル側の
//! ページフォールト（ディスク I/O）は `SF_NODISKIO` を指定しない限り同期的にブロックする**
//! （`man 2 sendfile` FreeBSD: "SF_NODISKIO ... 状態でなければ即座に応答を返さない" 相当の
//! 記述）。AGENTS.md のホットパス絶対規則（同期 I/O 禁止）に反するため、本実装は必ず
//! `SF_NODISKIO` を付与する。
//!
//! `SF_NODISKIO` 付きで対象範囲がまだ VM キャッシュに乗っていない場合、`sendfile` は
//! ブロックせず即座に `EBUSY` を返す（キャッシュ済みの先頭分だけ `sbytes` に返ることがある）。
//! この場合は `runtime::offload`（F-29 の専用スレッドプール、io_uring に対応オペコードが
//! 無いブロッキング処理をワーカースレッドへ退避する既存機構）で対象ページを 1 バイトだけ
//! `pread(2)` してキャッシュへ乗せてから再試行する。**イベントループ自体は一度もブロックしない**。
//!
//! # ヘッダ同時送出（`sf_hdtr`）について
//!
//! FreeBSD の `sendfile` は `sf_hdtr` でヘッダ/トレーラを本体と同一 syscall で送出できるが、
//! 部分送信（`EAGAIN`/`SF_NODISKIO` の `EBUSY` で送信済みバイト数が headers 途中で止まる場合）
//! の再試行時に「ヘッダの送信済み分だけ iovec を前進させ、送信済みなら `hdtr` を外す」という
//! 状態管理が必要になる。この状態管理を誤ると **クライアントへヘッダの重複送信/欠落**という
//! レスポンス破損（実機 FreeBSD でしか検証できない領域）につながる。
//!
//! F-155 では、この危険性を踏まえたうえで [`sendfile_all_with_header`] として実装した。
//! `header_sent`/`file_sent` の 2 変数だけを状態として持つステートマシンにし、1 回の
//! syscall が返す `sbytes`（ヘッダ + 本体の合計送信量）を「まず残りヘッダへ充当し、
//! 余りを本体へ充当する」という単純な算術（[`distribute_sbytes`]、syscall を含まない
//! 純関数として単体テスト可能）に還元することで、iovec 前進や `hdtr` 差し替えの分岐を
//! 減らし誤りの余地を小さくしている。適用範囲は `handle_sendfile`（`src/proxy.rs`）が
//! `ServerTls::is_plain()` で確認した **平文 HTTP/1.1 の静的配信のみ**であり、
//! `write_all`（ヘッダ）→ `sendfile_all`（本体）の従来 2-syscall 経路も
//! `handle_sendfile_userspace` に安全網として残している。

use std::io;
use std::os::unix::io::RawFd;

/// `SF_NODISKIO` 状態で `in_fd` の該当ページを VM キャッシュへ乗せる（専用スレッドで実行）。
///
/// 1 バイトだけ `pread` すればページフォールトが発生し、以降の `sendfile(2)` 再試行で
/// `SF_NODISKIO` が即座に成功できるようになる。戻り値やエラーは無視してよい
/// （`pread` が失敗しても、後続の `sendfile` 自体が改めてエラーを報告する）。
fn prefetch_page(fd: RawFd, offset: i64) {
    let mut byte = [0u8; 1];
    // SAFETY: byte は有効なスタックバッファ、fd は呼び出し側が生存を保証する。
    unsafe {
        libc::pread(
            fd,
            byte.as_mut_ptr() as *mut libc::c_void,
            1,
            offset as libc::off_t,
        );
    }
}

/// `in_fd`（ファイル）の `offset` から最大 `len` バイトを `out_fd`（ソケット）へ
/// ゼロコピー送信する。
///
/// 他の reactor Future（`WriteFuture`/`SendMsgFuture` 等）と同じ契約: 内部で
/// `EBUSY`（ページ未キャッシュ）/`EAGAIN`（送信バッファ満杯）を吸収して再試行し、
/// **実際に 1 バイト以上送信できた時点、または回復不能なエラー時のみ** `Ok`/`Err` を返す
/// （呼び出し側に spurious な待機理由を見せない）。`len == 0` の場合は `Ok(0)` を返す。
///
/// 戻り値は実際に送信できたバイト数（`len` 未満のことがある。ファイル終端などで
/// `sendfile` 自体が 0 バイトで正常終了した場合も `Ok(0)` を返すため、呼び出し側は
/// `write_all` 相当のループで残量を回すこと）。
pub async fn sendfile_once(
    out_fd: RawFd,
    in_fd: RawFd,
    offset: i64,
    len: usize,
) -> io::Result<usize> {
    if len == 0 {
        return Ok(0);
    }
    loop {
        let mut sbytes: libc::off_t = 0;
        // SAFETY: out_fd/in_fd は呼び出し側が有効性を保証する fd。hdtr は使わない
        // （モジュール doc 参照）ため NULL。sbytes は有効なスタック変数。
        let ret = unsafe {
            libc::sendfile(
                in_fd,
                out_fd,
                offset,
                len,
                std::ptr::null_mut(),
                &mut sbytes,
                libc::SF_NODISKIO,
            )
        };
        if ret == 0 {
            // 正常終了。sbytes==0 はファイル終端（EOF）等を意味し、呼び出し側の
            // write_all 相当ループが停止条件として扱う。
            return Ok(sbytes as usize);
        }
        let e = io::Error::last_os_error();
        if sbytes > 0 {
            // EAGAIN 等の直前まで部分送信が進んでいた分はそのまま返す。
            return Ok(sbytes as usize);
        }
        match e.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EBUSY) => {
                // SF_NODISKIO: 対象範囲が VM キャッシュに未ロード。専用スレッドで
                // 1 バイトだけ pread してページフォールトインしてから再試行する
                // （イベントループはブロックしない。モジュール doc 参照）。
                crate::runtime::offload::offload(move || prefetch_page(in_fd, offset)).await;
                continue;
            }
            Some(libc::EAGAIN) => {
                // ソケット送信バッファが満杯。writable になるまで待つ。
                crate::runtime::tcp::wait_writable_fd(out_fd).await?;
                continue;
            }
            _ => return Err(e),
        }
    }
}

/// `in_fd` の `offset` から `total_len` バイト全量を `out_fd` へゼロコピー送信する
/// （`sendfile_once` を用いた `write_all` 相当のループ）。
///
/// `sendfile_once` が `Ok(0)` を返した場合（ファイルが `total_len` に届く前に EOF に
/// 達した場合）は `UnexpectedEof` として扱う（呼び出し側の `Content-Length` 契約と
/// 矛盾する状態のため、黙って短く終わらせない）。
pub async fn sendfile_all(
    out_fd: RawFd,
    in_fd: RawFd,
    offset: i64,
    total_len: usize,
) -> io::Result<()> {
    let mut sent = 0usize;
    let mut cur_offset = offset;
    while sent < total_len {
        let n = sendfile_once(out_fd, in_fd, cur_offset, total_len - sent).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "sendfile: file ended before requested length was sent",
            ));
        }
        sent += n;
        cur_offset += n as i64;
    }
    Ok(())
}

/// 1 回の `sendfile(2)` 呼び出しが返した `sbytes`（ヘッダ + ファイル本体の合計送信量）を、
/// 残りヘッダ（`header_remaining`）とファイル本体へ配分する。
///
/// FreeBSD の `sendfile(2)` は `sf_hdtr` 使用時、`sbytes` にヘッダ・本体・トレーラの
/// 合計送信バイト数を返す契約であり、内訳は返してくれない。そこで「まず残りヘッダへ
/// 充当し、余りを本体へ回す」という単純な算術で内訳を復元する
/// （[`sendfile_all_with_header`] のモジュール doc 参照）。syscall を一切含まない
/// ため、実機 FreeBSD がなくても単体テストできる。ホットパスから毎周回呼ばれるため
/// `#[inline]` を付けアロケーション・間接呼び出しを増やさない。
///
/// 戻り値: `(このヘッダへの充当量, ファイル本体への充当量)`
#[inline]
fn distribute_sbytes(sbytes: usize, header_remaining: usize) -> (usize, usize) {
    let to_header = sbytes.min(header_remaining);
    let to_file = sbytes - to_header;
    (to_header, to_file)
}

/// `in_fd`（ファイル）の `offset` から `total_len` バイトと、`header` の全量を、
/// FreeBSD `sendfile(2)` の `sf_hdtr` 機構により **1 回の syscall** で `out_fd`
/// （ソケット）へ送信する（F-155）。
///
/// `write_all(header)` → `sendfile_all(body)` という従来の 2-syscall 経路を、
/// レスポンスヘッダをカーネルに `sf_hdtr` として渡すことで 1-syscall に統合する。
/// 呼び出し側（`src/proxy.rs::handle_sendfile`）は **平文 HTTP/1.1 の静的配信**
/// （`ServerTls::is_plain()` が真、かつ kTLS 無効）に限って本関数を使うこと。
/// rustls ユーザー空間 TLS や kTLS 経路でこの関数を使うと、ファイル本体が
/// 暗号化されずソケットへ流れる（平文漏洩）ため絶対に呼び出してはならない。
///
/// # 状態遷移
///
/// ループ状態は `header_sent`/`file_sent` の 2 変数のみ。各周回で
/// `header_sent < header.len()` の間だけ `sf_hdtr` にヘッダ残り分の 1 本 iovec を
/// 積み、送り切ったら `hdtr` に NULL を渡す（ヘッダの重複送信/欠落を防ぐため、
/// 一度でも `header_sent` が進んだらその分だけ iovec の先頭を前進させる）。
/// `nbytes`（本体の送信要求量）は `total_len - file_sent`、`offset` は
/// `offset + file_sent` を渡す。1 周回の `sbytes` は [`distribute_sbytes`] で
/// ヘッダ/本体へ配分する。
///
/// # エラー処理
///
/// `sendfile_once`/`sendfile_all` と同じ方針で `EINTR`/`EBUSY`（`SF_NODISKIO` の
/// ページ未キャッシュ）/`EAGAIN`（送信バッファ満杯）を吸収して再試行する。
/// ただし本関数は `write_all` 相当の全量送信 API のため、`sendfile_once` と異なり
/// **`sbytes > 0` でも直ちに `Ok` を返さず**、`header_sent == header.len() &&
/// file_sent == total_len` になるまでループを続ける。
///
/// 返り値 0（成功）にもかかわらず終了条件を満たさず、かつ `sbytes == 0` だった
/// 場合はファイルが `total_len` に届く前に EOF に達したことを意味するため、
/// `sendfile_all` と同様に `UnexpectedEof` を返し無限ループを防ぐ。
///
/// `total_len == 0` の場合、ヘッダのみを送るユースケースは `sendfile(2)` を使う
/// 利点がない（`write` 1 回で足りる）ため、本関数は `InvalidInput` を返す。
/// 呼び出し側は `total_len > 0` を保証してから呼ぶこと。
pub async fn sendfile_all_with_header(
    out_fd: RawFd,
    in_fd: RawFd,
    offset: i64,
    total_len: usize,
    header: &[u8],
) -> io::Result<()> {
    if total_len == 0 {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }

    let mut header_sent = 0usize;
    let mut file_sent = 0usize;

    while header_sent < header.len() || file_sent < total_len {
        let header_remaining = header.len() - header_sent;

        // iov/hdtr はこの周回のスタック変数。syscall 呼び出しの間だけ有効であれば
        // よく、await をまたいで保持しない（ループの各周回でここから再構築する）。
        let mut iov: libc::iovec;
        let mut hdtr: libc::sf_hdtr;
        let hdtr_ptr: *mut libc::sf_hdtr = if header_remaining > 0 {
            // SAFETY: header は本関数の呼び出し側（proxy.rs）が所有する Vec<u8> の
            // 借用であり、この async fn の呼び出し全体（本 await ポイントを含む）を
            // またいで生存することを型システム（&[u8] のライフタイム）が保証する。
            // header_sent < header.len() をループの直前で確認済みなので、
            // `add(header_sent)` はバッファ内（末尾ちょうども含む）を指す。
            iov = libc::iovec {
                iov_base: unsafe { header.as_ptr().add(header_sent) as *mut libc::c_void },
                iov_len: header_remaining,
            };
            hdtr = libc::sf_hdtr {
                headers: &mut iov as *mut libc::iovec,
                hdr_cnt: 1,
                trailers: std::ptr::null_mut(),
                trl_cnt: 0,
            };
            &mut hdtr as *mut libc::sf_hdtr
        } else {
            std::ptr::null_mut()
        };

        let nbytes = total_len - file_sent;
        // FreeBSD の `sendfile(2)` は **`nbytes == 0` を「ファイル終端まで送る」**と
        // 解釈する（`man 2 sendfile`）。`sf_hdtr` はヘッダを本体より先に送るため
        // 「本体を送り切ったのにヘッダが残っている」状態は原理上ありえず、ここは
        // 到達しないはずだが、万一到達すると Content-Length を超えるバイト列を
        // 送ってレスポンスを破壊する。不変条件が破れたことを検出して打ち切る。
        if nbytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "sendfile: header remains after body was fully sent (sf_hdtr invariant violated)",
            ));
        }
        let file_offset = offset + file_sent as i64;
        let mut sbytes: libc::off_t = 0;

        // SAFETY: out_fd/in_fd は呼び出し側が有効性を保証する fd。hdtr_ptr は
        // header_remaining > 0 のときのみ非 NULL であり、その場合は直前で構築した
        // iov/hdtr（このスコープ内で生存）を指す。sendfile(2) は syscall 実行中
        // だけこれらを読むため、await をまたいで保持する必要はない。sbytes は
        // 有効なスタック変数。
        let ret = unsafe {
            libc::sendfile(
                in_fd,
                out_fd,
                file_offset,
                nbytes,
                hdtr_ptr,
                &mut sbytes,
                libc::SF_NODISKIO,
            )
        };

        if ret == 0 {
            // 成功。sbytes をまず回収してからヘッダ/本体へ配分する。
            let (to_header, to_file) = distribute_sbytes(sbytes as usize, header_remaining);
            header_sent += to_header;
            file_sent += to_file;
            let finished = header_sent == header.len() && file_sent == total_len;
            if sbytes == 0 && !finished {
                // ループ突入条件（while 節）よりこの周回開始時点では未完了だったこと
                // は保証済み。にもかかわらず進捗ゼロで完了もしていない = ファイルが
                // total_len に届く前に EOF。無限ループ防止のため打ち切る。
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "sendfile: file ended before requested length was sent",
                ));
            }
            continue;
        }

        // errno を見る前に、まず sbytes（部分送信済み量）を回収して反映する。
        let e = io::Error::last_os_error();
        if sbytes > 0 {
            let (to_header, to_file) = distribute_sbytes(sbytes as usize, header_remaining);
            header_sent += to_header;
            file_sent += to_file;
            // write_all 相当のため、部分送信でも return せずループを継続する。
            continue;
        }
        match e.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EBUSY) => {
                // SF_NODISKIO: 対象範囲が VM キャッシュに未ロード。次に読むべき
                // ファイルオフセットは offset + file_sent（sendfile_once 参照）。
                let off = offset + file_sent as i64;
                crate::runtime::offload::offload(move || prefetch_page(in_fd, off)).await;
                continue;
            }
            Some(libc::EAGAIN) => {
                crate::runtime::tcp::wait_writable_fd(out_fd).await?;
                continue;
            }
            _ => return Err(e),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::distribute_sbytes;

    #[test]
    fn distribute_sbytes_all_into_header_when_header_remaining_covers_it() {
        assert_eq!(distribute_sbytes(10, 20), (10, 0));
    }

    #[test]
    fn distribute_sbytes_splits_across_header_and_file() {
        assert_eq!(distribute_sbytes(30, 20), (20, 10));
    }

    #[test]
    fn distribute_sbytes_all_into_file_when_no_header_remaining() {
        assert_eq!(distribute_sbytes(50, 0), (0, 50));
    }

    #[test]
    fn distribute_sbytes_zero_progress() {
        assert_eq!(distribute_sbytes(0, 20), (0, 0));
        assert_eq!(distribute_sbytes(0, 0), (0, 0));
    }

    #[test]
    fn distribute_sbytes_exact_header_boundary() {
        assert_eq!(distribute_sbytes(20, 20), (20, 0));
    }
}
