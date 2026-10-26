# B-187: ストリーミング受信のハンドラに、途中で切れた本文が正常な終わりとして届く

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 のデータプレーンのレビュー（`docs/artifacts/v0100/review-dataplane.md` の M-3）。F-266（HTTP/1.1）・F-197（HTTP/2）・
F-209（HTTP/3）。

## 現象

ストリーミング受信のルート（`streaming = true`）で、アップロードの途中でクライアントが切断する・ストリームをリセットする・
`READ_TIMEOUT` が切れると、ハンドラの `take_body_stream()` は `None`（正常な終わり）になる。生の本文のストリームを
ファイルへ書くハンドラは、切れたファイルを完成品として保存・確定してしまう（multipart は終端の区切りが無いことで
気付けるが、生のストリームでは気付けない）。HTTP/1.1 は Content-Length が分かっているのに区別していなかった。
`prepare_form`（C・Python のバインディングがストリーミングでないハンドラの前に本文を溜める）も、切れた本文を
`Request::body` に入れていた。

## 原因

`RequestBodyStream` は `Item = Bytes` だけで終わりの理由を持たず、送り手（HTTP/1.1 の `pump_api_body`、HTTP/2・HTTP/3 の
中継）は途中で切れても送信端を落とすだけだった。HTTP/2・HTTP/3 の接続側のチャネル（`stream_channel`）も、END_STREAM・fin
での正常な終わりとリセット・切断での終わりを区別しなかった。

## 直したこと

- `RequestBodyStream::is_aborted()`（公開 API の追加。`Item = Bytes` は変えない）: 終わり（`None`）の後に、本文が途中で
  切れたかを返す。送り手は `RequestBodySender`（`request_body_channel` が返す）で、`abort()` で切れた終わりにする。
- HTTP/1.1: `pump_api_body` は Content-Length に届かずに読み取りが終わった（EOF・エラー・時間切れ）ら `abort()`。
- HTTP/2・HTTP/3: `stream_channel::Sender::finish()` を足し、END_STREAM・fin で全部渡したときだけ `finish` で閉じる。
  それ以外（リセット・切断・上限超過）で閉じたものは `Receiver::is_cut_off()` が真になり、ハンドラへの中継が `abort()` する。
- 内部の利用者: gRPC の要求のストリーム（`Streaming`）は切れた終わりを `CANCELLED` のエラーにする。`prepare_form` は
  400 にする。`Multipart` は今までどおり終端の区切りが無いことでエラーになる。
- 文書: `docs/guide/library.md`（英日）に `is_aborted` を書いた。
- 残り: C・Python の `HandlerContext::read_body`（ストリーミング受信の本文を順に読む）は終わりしか返さない（切れたかを
  知る口は無い）。C・Python へ出すときに足す。HTTP/2 の上流への中継（プロキシ）は、クライアントがリセットした本文を
  上流へ END_STREAM で送るかどうかを別に確かめる。

## 試験

- 単体 `api::stream::tests::request_body_channel_reports_abort`、`proxy::h1::b186_overflow_tests::pump_api_body_returns_pipelined_bytes_and_reports_abort`
  （Content-Length の途中でクライアントが送信を閉じると `is_aborted()` が真。直す前は区別できない）。
