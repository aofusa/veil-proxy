# B-286: Linux の平文の HTTP/1.1 がファイルと上流の本文をユーザー空間へ読んで送り直している（文書は sendfile と書いている）

**状態: 修正済み（v0.10.1）**

## 出典

veil v0.9.2 を Go・C#・F#・Scala・Haskell へ移植して比べた計測（別のセッション、aofish の Docker、2026-11-04）。
平文の HTTP/1.1 では、ゼロコピーで送る移植のほうが veil より 1.2〜1.5 倍速かった（54KB のファイル・プロキシ）。
strace で見た v0.9.2 の経路は、ファイルが `pread64` + `sendto`、プロキシが `read` + `sendmsg` だった。

## 事象

Linux の平文（TLS 終端なし。`[server] listen_cleartext`）の HTTP/1.1 で、

- 静的配信（`type = "File"`、`mode = "sendfile"`）の本文を `pread` でユーザー空間へ読み、`send` で書き直していた。
- プロキシの Content-Length の本文を上流から `read` し、クライアントへ `sendmsg` で書き直していた。

`sendfile(2)`・`splice(2)` を使っていたのは kTLS の HTTP/1.1 と FreeBSD の平文の静的配信だけだった。文書
（`docs/guide/features.md`・`configuration.md`・`examples/config.toml` の `[static_file_cache]`）は「平文の HTTP/1.1 は
`sendfile(2)` のゼロコピー経路を使う」と書いており、実装と食い違っていた。

## 原因

ゼロコピーの経路は kTLS の機能として作られ（`handle_sendfile_zerocopy`・`try_splice_proxy` の条件が `is_ktls_enabled()`）、
平文の接続を入れていなかった。FreeBSD の平文には F-141・F-155 で `sendfile(2)` を足したが、Linux には足していなかった。
コメント・コミット・チケットに、平文を外す理由は書かれていない（seccomp は `sendfile`・`splice`・`pipe2` を許可済み、
受理したソケットは `SOCK_NONBLOCK`）。

## 修正（v0.10.1）

- **静的配信**（`src/proxy/sendfile.rs` の `sendfile_plain`）: 平文の接続はヘッダを `MSG_MORE` 付きの `send(2)` で送り
  （`pool::async_raw_send_more_all`。本文の最初のセグメントとまとまる）、本文を `sendfile(2)` で送る。送信バッファが一杯なら
  `EAGAIN` で戻り、書き込み可能を待って続きから送る（`WRITE_TIMEOUT`）。長さに届かずに終わったら接続を閉じる。ファイルが
  `sendfile(2)` に対応しない（`EINVAL` 等で 1 バイトも送れない）ときは本文だけ従来の読み書きで送る。静的コンテンツキャッシュに
  ある本文は従来どおりキャッシュから 1 回の書き込みで送る（検証子と同じ版の中身を返す。監視しないルートは有効期限まで古い中身の
  まま、という他のプロトコルと同じ振る舞い。F-272 の試験）。rustls の接続は通らない（`is_plain()`）。
- **プロキシ**（`src/proxy/splice.rs`・`forward.rs`）: kTLS の splice の経路（Content-Length の本文、圧縮・キャッシュ保存・
  `buffering`・Wasm・CORS の無いルート）を平文の接続にも使う。平文では `splice(2)` を io_uring に出さず、その場で非ブロッキングで
  呼ぶ（`splice_now`）。io_uring の `IORING_OP_SPLICE` はソケットが相手でも常にカーネルのワーカースレッドへ回され、そのままでは
  54KB で 0.78 倍に落ちた。応答ヘッダは本文が続くときだけ `MSG_MORE` で送る。読み取り済みの未消費データを抱えた接続は通常の
  経路へ戻す。kTLS の経路（io_uring の splice・`MSG_MORE` を付けない）は変えていない。
- HTTP/2 は変えない（移植の計測で、DATA フレームごとの splice は 1 回の writev のコピーより遅かった）。

## 確認

- 統合試験 `plaintext_h1_zero_copy_files_and_proxy_bodies_keep_framing`（`tests/standalone/parity_tests.rs`）: 3MB のファイル
  （速い・遅い受け手）・3B・Range（途中から・末尾）・静的コンテンツキャッシュのあるルート・上流の 2MB の応答・1MB の要求の本文を
  同じキープアライブ接続で続けて送り、中身と区切りが崩れない。
- strace（epoll、500 要求）: ファイルは `pread64` + `sendto` 2 回 → `sendto`（ヘッダ）+ `sendfile`。プロキシは `read`（本文）+
  `sendmsg` → `splice` 2 回。
- aofish の Docker、平文の HTTP/1.1（wrk -t4 -c100、v0.10.1 の候補との交互 4 回の中央値）:

  | 構成 | 54KB | 3B |
  |---|---|---|
  | ファイル（キャッシュ無し） | 1.40 倍（CPU/要求 199 → 137µs） | 1.10 倍（104 → 89µs） |
  | プロキシ | 1.11 倍（128 → 122µs） | 0.96 倍（71 → 72µs、前回の計測は 1.12 倍。揺れの範囲） |

## セキュリティレビュー（Fable 5.1、2026-11-04）

Critical・High は無し（rustls の接続が sendfile・splice・生の書き込みに届く経路は無い、kTLS の振る舞いは不変、本文の区切りは
正しい）。指摘を同じ変更で直した。

- Medium: 平文の `sendfile(2)` のループが譲らず、速く受け取る相手へ巨大なファイルを送る間、同じワーカーの他の接続が止まる →
  1 回 1MB までにし、送り切るたびに `yield_now`。
- Low: 平文の splice の経路の応答 head の `MSG_MORE` の書き込みに時間の上限が無い → `WRITE_TIMEOUT`。
- Low: ファイルの読み書きの経路で本文が長さに届かずに終わっても接続を閉じない（以前から。rustls・`sendfile(2)` 非対応の
  ファイル）→ 閉じる。
- Low: `sendfile(2)`・`splice(2)` は `MSG_NOSIGNAL` を付けられず、SIGPIPE を無視しない C のホストが落ち得る →
  `App::run`（`veil_run`）で SIGPIPE を無視する（`docs/guide/library.md`）。

報告は `docs/artifacts/security/v0101-fable-review-b286.md`。

## 残り

- kTLS の splice の経路も io_uring の `IORING_OP_SPLICE`（常にワーカースレッド）を使っている。平文と同じくその場の
  非ブロッキングの splice にすると速くなる見込みだが、kTLS のソケットへの splice はカーネルの差が出やすい（WSL のカーネルで
  `MSG_MORE` と splice の組み合わせで VM ごと落ちた報告がある）ため、本チケットでは変えない。

## 関連

- [F-141](../features/F-141-freebsd-kqueue-aio-optimization.md)・[F-155](../features/F-155-freebsd-perf-nginx-parity.md)（FreeBSD の平文の `sendfile(2)`）、
  [F-150](../features/F-150-rustls-writev-zero-copy-send.md)（静的コンテンツキャッシュ）、[B-25](B-25-reverse-proxy-http1-wrk-zero-completed.md)（kTLS と `SPLICE_F_MORE`）
