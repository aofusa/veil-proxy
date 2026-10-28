// F-262 第 2 段: permessage-deflate（RFC 7692）。`[api] websocket_compression = true` かつ `compression` feature のとき。
// サーバ側は常に server_no_context_takeover にする（圧縮器をメッセージごとに初期化するので、接続ごとに圧縮の
// 状態（数百 KB）を持たずスレッドで 1 個を使い回せる）。展開はクライアントの文脈の引き継ぎに従う。
//! permessage-deflate (RFC 7692).

use bytes::Bytes;
use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress};
use std::cell::RefCell;

/// The negotiated parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Params {
    /// The client resets its compression context per message (so the inflater can be reset too).
    pub client_no_context_takeover: bool,
}

/// Picks the first acceptable `permessage-deflate` offer of `Sec-WebSocket-Extensions` and returns the response
/// value. Offers asking for a server window smaller than 15 bits are declined (the inflate/deflate backend always
/// uses 15 bits).
pub(crate) fn negotiate<'a>(values: impl Iterator<Item = &'a [u8]>) -> Option<(String, Params)> {
    for v in values {
        let Ok(v) = std::str::from_utf8(v) else {
            continue;
        };
        for offer in v.split(',') {
            if let Some(p) = accept_offer(offer) {
                let mut resp = String::from("permessage-deflate; server_no_context_takeover");
                if p.client_no_context_takeover {
                    resp.push_str("; client_no_context_takeover");
                }
                return Some((resp, p));
            }
        }
    }
    None
}

fn accept_offer(offer: &str) -> Option<Params> {
    let mut parts = offer.split(';').map(str::trim);
    if !parts.next()?.eq_ignore_ascii_case("permessage-deflate") {
        return None;
    }
    let mut seen = [false; 4];
    let mut p = Params {
        client_no_context_takeover: false,
    };
    for param in parts {
        if param.is_empty() {
            return None;
        }
        let (name, value) = match param.split_once('=') {
            Some((n, v)) => (n.trim(), Some(v.trim().trim_matches('"'))),
            None => (param, None),
        };
        let idx = match name.to_ascii_lowercase().as_str() {
            "server_no_context_takeover" if value.is_none() => 0,
            "client_no_context_takeover" if value.is_none() => {
                p.client_no_context_takeover = true;
                1
            }
            "server_max_window_bits" => {
                // 15 以外は断る（窓を小さくできない）
                if value?.parse::<u8>().ok()? != 15 {
                    return None;
                }
                2
            }
            "client_max_window_bits" => {
                // 値は無くてもよい。8〜15 なら 15 の窓で展開できる（応答には書かない = クライアントは 15 まで使える）
                if let Some(v) = value {
                    if !(8..=15).contains(&v.parse::<u8>().ok()?) {
                        return None;
                    }
                }
                3
            }
            _ => return None,
        };
        if std::mem::replace(&mut seen[idx], true) {
            return None;
        }
    }
    Some(p)
}

// これより短いメッセージは圧縮しない（ヘッダと同期の 4 バイトで得にならない）。
pub(crate) const COMPRESS_MIN: usize = 64;
const TAIL: [u8; 4] = [0x00, 0x00, 0xff, 0xff];

thread_local! {
    // server_no_context_takeover なのでメッセージごとに初期化して使い回す
    static COMPRESSOR: RefCell<Compress> = RefCell::new(Compress::new(Compression::fast(), false));
}

/// Compresses one message (raw deflate, sync flush, without the trailing `00 00 ff ff`); `None` when it would
/// not fit the worst-case output size (the message is then sent uncompressed, which RFC 7692 allows).
pub(crate) fn compress(payload: &[u8]) -> Option<Bytes> {
    COMPRESSOR.with(|c| {
        let mut c = c.borrow_mut();
        c.reset();
        // 出力は最悪の大きさ（格納ブロックの見出し・同期の区切りを含む）を最初に確保して 1 回で圧縮する。
        // flate2（miniz_oxide）は出力が足りずに複数回に分けて呼ぶと中身が欠けることがある
        // （Autobahn 12.2.9・12.5.* で発覚。出力の余裕が十分なら起きない）
        let mut out: Vec<u8> = Vec::with_capacity(payload.len() + payload.len() / 8 + 64);
        let st = c.compress_vec(payload, &mut out, FlushCompress::Sync);
        if st.is_err() || c.total_in() as usize != payload.len() || out.len() == out.capacity() {
            return None;
        }
        if out.ends_with(&TAIL) {
            out.truncate(out.len() - 4);
        }
        Some(Bytes::from(out))
    })
}

/// The inflater of one connection.
pub(crate) struct Inflater {
    d: Decompress,
    reset_each: bool,
}

impl Inflater {
    pub(crate) fn new(p: Params) -> Inflater {
        Inflater {
            d: Decompress::new(false),
            reset_each: p.client_no_context_takeover,
        }
    }

    /// Inflates one message; `Err(1009)` when the output exceeds `max`, `Err(1007)` when the data is not valid
    /// deflate.
    pub(crate) fn inflate(&mut self, data: &[u8], max: usize) -> Result<Bytes, u16> {
        if self.reset_each {
            self.d.reset(false);
        }
        let mut out: Vec<u8> =
            Vec::with_capacity((data.len() * 4).clamp(256, 64 * 1024).min(max + 1));
        'inputs: for input in [data, &TAIL[..]] {
            let start_in = self.d.total_in();
            loop {
                let consumed = (self.d.total_in() - start_in) as usize;
                if out.len() == out.capacity() {
                    out.reserve(out.capacity().clamp(256, 1 << 20));
                }
                let before_out = out.len();
                let st = self
                    .d
                    .decompress_vec(&input[consumed..], &mut out, FlushDecompress::Sync)
                    .map_err(|_| super::codec::CLOSE_INVALID_DATA)?;
                if out.len() > max {
                    return Err(super::codec::CLOSE_TOO_BIG);
                }
                let done_in = (self.d.total_in() - start_in) as usize == input.len();
                if st == flate2::Status::StreamEnd {
                    // BFINAL のブロックの後ろは無い（文脈の引き継ぎ中なら次のメッセージは新しい流れになる）。
                    // B-199: 末尾の 00 00 ff ff を新しい展開器に流さない（格納ブロックの途中として持ち越し、
                    // 次のメッセージの先頭が NLEN の続きに読まれて 1007 になる）
                    self.d.reset(false);
                    break 'inputs;
                }
                if done_in && out.len() < out.capacity() {
                    break;
                }
                if out.len() == before_out && !done_in && out.len() < out.capacity() {
                    // 進まない（壊れたデータ）
                    return Err(super::codec::CLOSE_INVALID_DATA);
                }
            }
        }
        Ok(Bytes::from(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation() {
        let n = |s: &str| negotiate(std::iter::once(s.as_bytes()));
        let (r, p) = n("permessage-deflate; client_max_window_bits").unwrap();
        assert_eq!(r, "permessage-deflate; server_no_context_takeover");
        assert!(!p.client_no_context_takeover);
        let (r, p) =
            n("permessage-deflate; client_no_context_takeover; server_max_window_bits=15").unwrap();
        assert_eq!(
            r,
            "permessage-deflate; server_no_context_takeover; client_no_context_takeover"
        );
        assert!(p.client_no_context_takeover);
        // 窓 10 は断り、次の申し出を受ける
        let (r, _) =
            n("permessage-deflate; server_max_window_bits=10, permessage-deflate").unwrap();
        assert_eq!(r, "permessage-deflate; server_no_context_takeover");
        assert!(n("permessage-deflate; server_max_window_bits=10").is_none());
        assert!(n("permessage-deflate; unknown").is_none());
        assert!(
            n("permessage-deflate; client_no_context_takeover; client_no_context_takeover")
                .is_none()
        );
        assert!(n("x-webkit-deflate-frame").is_none());
        assert!(n("permessage-deflate; client_max_window_bits=7").is_none());
    }

    #[test]
    fn roundtrip_and_limits() {
        let text = "hello hello hello hello hello hello hello hello hello hello hello".repeat(20);
        let c = compress(text.as_bytes()).unwrap();
        assert!(c.len() < text.len());
        let mut inf = Inflater::new(Params {
            client_no_context_takeover: false,
        });
        assert_eq!(&inf.inflate(&c, 1 << 20).unwrap()[..], text.as_bytes());
        // 同じ展開器で 2 通目（文脈を引き継いでも、サーバ側は毎回初期化して圧縮している）
        assert_eq!(&inf.inflate(&c, 1 << 20).unwrap()[..], text.as_bytes());
        assert_eq!(
            inf.inflate(&c, 100),
            Err(crate::api::ws::codec::CLOSE_TOO_BIG)
        );
        let mut inf = Inflater::new(Params {
            client_no_context_takeover: true,
        });
        assert_eq!(
            inf.inflate(&[0xff, 0xff, 0xff], 1 << 20),
            Err(crate::api::ws::codec::CLOSE_INVALID_DATA)
        );
        // 空のメッセージ
        let e = compress(b"").unwrap();
        assert_eq!(&inf.inflate(&e, 10).unwrap()[..], b"");
        // 圧縮率の高い入力（zip bomb）は上限で止まる
        let zeros = vec![0u8; 4 << 20];
        let c = compress(&zeros).unwrap();
        assert!(c.len() < 64 * 1024);
        assert_eq!(
            inf.inflate(&c, 1 << 20),
            Err(crate::api::ws::codec::CLOSE_TOO_BIG)
        );
    }

    // B-199: BFINAL で終わるメッセージの後、文脈を引き継ぐ接続で次のメッセージも展開できる
    #[test]
    fn bfinal_message_then_next_message() {
        let finished = |data: &[u8]| {
            let mut c = Compress::new(Compression::default(), false);
            let mut out = Vec::with_capacity(data.len() + 64);
            c.compress_vec(data, &mut out, FlushCompress::Finish)
                .unwrap();
            out
        };
        let a = finished(b"first message");
        let b = finished(b"second message");
        let mut inf = Inflater::new(Params {
            client_no_context_takeover: false,
        });
        assert_eq!(&inf.inflate(&a, 1 << 20).unwrap()[..], b"first message");
        assert_eq!(&inf.inflate(&b, 1 << 20).unwrap()[..], b"second message");
        // 同期の区切りで終わる普通のメッセージがその後に続いても良い
        let c = compress(b"third").unwrap();
        assert_eq!(&inf.inflate(&c, 1 << 20).unwrap()[..], b"third");
    }

    #[test]
    fn large_messages_roundtrip() {
        // 圧縮できない（乱数）・できる 64 KiB 以上のメッセージが欠けずに往復する（Autobahn 12.*）
        let mut x: u32 = 0x1234_5678;
        let random: Vec<u8> = (0..(256 << 10))
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        let text: Vec<u8> = (0..(256 << 10))
            .map(|i| b"abcdefghij"[i % 7 + (i / 1000) % 3])
            .collect();
        for data in [
            &random[..65536],
            &random[..],
            &text[..65536],
            &text[..],
            &random[..4097],
        ] {
            let c = compress(data).unwrap();
            let mut inf = Inflater::new(Params {
                client_no_context_takeover: false,
            });
            let got = inf.inflate(&c, 1 << 20).unwrap();
            assert_eq!(got.len(), data.len());
            assert_eq!(&got[..], data);
            // 別の展開器（flate2 の素の使い方）でも同じ
            let mut d = Decompress::new(false);
            let mut out = Vec::with_capacity(data.len() + 1024);
            let mut input = c.to_vec();
            input.extend_from_slice(&TAIL);
            d.decompress_vec(&input, &mut out, FlushDecompress::Sync)
                .unwrap();
            assert_eq!(out.len(), data.len());
        }
        // クライアントが文脈を引き継いで（既定の申し出）続けて送る大きなメッセージ
        let mut client = Compress::new(Compression::default(), false);
        let mut inf = Inflater::new(Params {
            client_no_context_takeover: false,
        });
        for i in 0..50 {
            let data = if i % 2 == 0 {
                &random[i * 1000..i * 1000 + 65536]
            } else {
                &text[i * 100..i * 100 + 65536]
            };
            let mut c = Vec::with_capacity(data.len() * 2);
            client
                .compress_vec(data, &mut c, FlushCompress::Sync)
                .unwrap();
            assert!(c.ends_with(&TAIL));
            c.truncate(c.len() - 4);
            let got = inf.inflate(&c, 1 << 20).unwrap();
            assert_eq!(got.len(), data.len(), "message {}", i);
            assert_eq!(&got[..], data);
        }
    }
}
