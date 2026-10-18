//! 静的コンテンツキャッシュの本体を無名の共有メモリ（`shm_open(SHM_ANON)`）に置く（FreeBSD 専用、v0.8.0）。
//!
//! FreeBSD の `sendfile(2)` は共有メモリのオブジェクトからも送れる（13.0 以降）。キャッシュの本体を共有メモリに
//! 置いておくと、平文の h2c の DATA フレームの本体を `sendfile(2)`（フレームのヘッダは `sf_hdtr`）で送れる
//! （`http2::connection` のゼロコピーの送出）。`write(2)` だとユーザ空間のコピーに加えてカーネルがソケットの
//! バッファへもう 1 回コピーし、受け手のクライアントにも普通の mbuf のクラスタで渡るが、`sendfile(2)` はページを
//! 参照するだけで済む（FreeBSD aarch64 の h2c 54KB で、nginx は sendfile を切ると 110k → 76k req/s。veil は 95k）。
//!
//! 共有メモリの領域は読み取り専用で `mmap` し、[`bytes::Bytes`] の持ち主にする。キャッシュから返す `Bytes`（と、
//! その `slice`）がどの領域のどこを指すかは、[`lookup`] で引ける（領域の表は `ArcSwap` のスナップショットで、
//! 引くときにロックを取らない。表の更新はキャッシュへの挿入・破棄のときだけ）。

use arc_swap::ArcSwap;
use bytes::Bytes;
use once_cell::sync::Lazy;
use std::os::unix::io::RawFd;

/// これより小さい本体は共有メモリに置かない（ページの単位で確保するので、小さいファイルは無駄が大きい。
/// ゼロコピーの送出もフレーム 1 つぶん以上でないと効かない）
pub(crate) const SHM_MIN_LEN: usize = 16 * 1024;

#[derive(Clone, Copy)]
struct Region {
    start: usize,
    len: usize,
    fd: RawFd,
}

/// 生きている領域の表（`start` の昇順）
static REGIONS: Lazy<ArcSwap<Vec<Region>>> = Lazy::new(|| ArcSwap::from_pointee(Vec::new()));

fn register(r: Region) {
    REGIONS.rcu(|cur| {
        let mut v: Vec<Region> = (**cur).clone();
        let i = v.partition_point(|x| x.start < r.start);
        v.insert(i, r);
        v
    });
}

fn unregister(start: usize) {
    REGIONS.rcu(|cur| {
        let mut v: Vec<Region> = (**cur).clone();
        v.retain(|x| x.start != start);
        v
    });
}

/// 共有メモリの領域の持ち主（`Bytes::from_owner` に渡す）。drop で `munmap`・`close`・表から外す。
struct ShmRegion {
    ptr: *const u8,
    len: usize,
    fd: RawFd,
}

// SAFETY: 領域は読み取り専用の mmap で、作った後は誰も書き込まない。ポインタは drop までの間ずっと有効。
unsafe impl Send for ShmRegion {}
// SAFETY: 同上（共有されるのは読み取りだけ）。
unsafe impl Sync for ShmRegion {}

impl AsRef<[u8]> for ShmRegion {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: ptr は len バイトの読み取り可能な mmap 領域を指し、self が生きている間は munmap しない。
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for ShmRegion {
    fn drop(&mut self) {
        unregister(self.ptr as usize);
        // SAFETY: ptr/len は new で mmap した範囲そのもの、fd は new で開いた共有メモリ。どちらも 1 回だけ解放する。
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.len);
            libc::close(self.fd);
        }
    }
}

/// `data` を共有メモリへ写した `Bytes` を返す。小さい・失敗したときは `data` をそのまま返す。
///
/// キャッシュへの挿入（ミスのとき）だけから呼ぶ。コピーは 1 回だけで、以後のヒットは参照カウントの
/// クローンのまま。
pub(crate) fn to_shm(data: Bytes) -> Bytes {
    if data.len() < SHM_MIN_LEN {
        return data;
    }
    match make_region(&data) {
        Some(r) => Bytes::from_owner(r),
        None => data,
    }
}

fn make_region(data: &[u8]) -> Option<ShmRegion> {
    let len = data.len();
    // SAFETY: SHM_ANON は無名の共有メモリを作る。成功すれば所有する fd が返る。
    let fd = unsafe { libc::shm_open(libc::SHM_ANON, libc::O_RDWR, 0o600) };
    if fd < 0 {
        return None;
    }
    let close = |fd| {
        // SAFETY: 自分で開いた fd を 1 回だけ閉じる。
        unsafe { libc::close(fd) };
    };
    // SAFETY: fd は直前に開いた共有メモリ。
    if unsafe { libc::ftruncate(fd, len as libc::off_t) } != 0 {
        close(fd);
        return None;
    }
    let mut written = 0usize;
    while written < len {
        // SAFETY: data[written..] は有効なスライス、fd は書き込み可能な共有メモリ。
        let n = unsafe {
            libc::pwrite(
                fd,
                data[written..].as_ptr() as *const libc::c_void,
                len - written,
                written as libc::off_t,
            )
        };
        if n <= 0 {
            close(fd);
            return None;
        }
        written += n as usize;
    }
    // SAFETY: len バイトに伸ばした共有メモリを読み取り専用で共有の写像にする。
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        close(fd);
        return None;
    }
    let ptr = ptr as *const u8;
    register(Region {
        start: ptr as usize,
        len,
        fd,
    });
    Some(ShmRegion { ptr, len, fd })
}

/// `data` が共有メモリの領域の中なら、その共有メモリの fd と領域の先頭からのオフセットを返す。
///
/// 呼び出し側は、送り終えるまで `data` の持ち主（`Bytes`）を保持すること（保持している間は領域も fd も生きている）。
#[inline]
pub(crate) fn lookup(data: &[u8]) -> Option<(RawFd, u64)> {
    let p = data.as_ptr() as usize;
    let regions = REGIONS.load();
    let i = regions.partition_point(|r| r.start <= p);
    let r = regions.get(i.checked_sub(1)?)?;
    let end = p.checked_add(data.len())?;
    (end <= r.start + r.len).then(|| (r.fd, (p - r.start) as u64))
}

/// テスト用: その先頭の領域が表にあるか（ほかのテストが並行して領域を作っても影響されない）
#[cfg(test)]
fn has_region(start: usize) -> bool {
    REGIONS.load().iter().any(|r| r.start == start)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shm_bytes_round_trip_and_lookup() {
        let src: Vec<u8> = (0..(SHM_MIN_LEN * 3)).map(|i| (i % 251) as u8).collect();
        let b = to_shm(Bytes::from(src.clone()));
        let start = b.as_ptr() as usize;
        assert_eq!(&b[..], &src[..]);
        let (fd, off) = lookup(&b).expect("shm");
        assert!(fd >= 0);
        assert_eq!(off, 0);
        let s = b.slice(1000..5000);
        assert_eq!(lookup(&s).map(|x| x.1), Some(1000));
        assert!(lookup(b"not shm").is_none());
        assert!(has_region(start));
        drop(s);
        drop(b);
        assert!(!has_region(start));
        // 小さいものは写さない
        let small = to_shm(Bytes::from_static(b"tiny"));
        assert!(lookup(&small).is_none());
    }
}
