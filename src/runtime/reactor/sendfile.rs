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
//! レスポンス破損（実機 FreeBSD でしか検証できない領域）につながるため、本実装では
//! **意図的に採用しない**（`docs/backlog/features/F-141-freebsd-kqueue-aio-optimization.md`
//! に理由を記録）。ヘッダはこれまで通り通常の `write`/`sendmsg` 経路で送り、ファイル本体
//! （ヘッダより遥かに大きく、ゼロコピー化の効果が大きい部分）のみを本モジュールで送る。

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
