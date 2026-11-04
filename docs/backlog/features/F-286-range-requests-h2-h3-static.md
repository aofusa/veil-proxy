# F-286: HTTP/2・HTTP/3 の静的配信で Range 要求（206・416・`Accept-Ranges`）に応える（今は HTTP/1.1 の `type = "File"` だけ）

**状態: 未着手（2026-11-04 起票）**

## 今の状態

[B-266](../bugs/B-266-head-local-responses-send-body.md) と同じ調査で、v0.10.0 のソースのデバッグビルドに `Range: bytes=0-9` を送って確かめた。

| | HTTP/1.1 | HTTP/2（h2c・TLS） | HTTP/3 |
|---|---|---|---|
| `type = "File"` | 206 で 10 バイト、`Accept-Ranges: bytes`、範囲外は 416（`sendfile::handle_sendfile`） | **200 で全体**、`Accept-Ranges` 無し（実機） | **200 で全体**（コード。Range を見ない） |
| `mode = "memory"` | **200 で全体** | **200 で全体** | **200 で全体** |

- ブラウザは普通 HTTP/2・HTTP/3 で来るので、動画・音声のシーク、大きなファイルのダウンロードの再開が効かない（毎回先頭から全部送る）。
- ライブラリの文書（`library-kit.md` の `serve::dir`）は「sendfile, kTLS, Range and ETag work as for the proxy」と書くが、Range が効くのは
  HTTP/1.1 だけ（ETag はどこにも無い。[F-287](F-287-static-etag-conditional-requests.md)）。

## 改修内容

- Range の解釈（`http_utils` の `parse_range_header`・`normalize_range`）を共有し、HTTP/2・HTTP/3 の静的配信で 1 つの範囲の要求に 206 と
  `Content-Range` で応える。範囲外は 416 と `Content-Range: bytes */<size>`。複数の範囲（multipart/byteranges）は今の HTTP/1.1 と同じく
  最初の範囲だけにするか 200 にするかを決めて 3 つでそろえる。
- 本文は `static_file_cache` の `Bytes` を `slice` して渡す（コピーしない）。キャッシュに載らない大きなファイルは範囲の分だけ読む（offload）。
- `mode = "memory"` も同じにする（`Arc<Vec<u8>>` を切り出す）。
- 圧縮して返すときは Range を見ずに 200 で全体を返し、`Accept-Ranges` を付けない（[B-269](../bugs/B-269-h1-static-files-not-compressed.md) と同じ方針）。
- `If-Range` は [F-287](F-287-static-etag-conditional-requests.md) の検証子ができてから。

## 受け入れ条件

- E2E: HTTP/1.1・h2c・HTTP/2・HTTP/3 × File・memory で、`bytes=0-9`・`bytes=-10`・`bytes=10-`・範囲外の結果が同じ。HEAD とも組み合わせる。
- 交互 A/B: Range の無い静的配信の性能が変わらない（3B・54KB）。

## 関連

- [B-266](../bugs/B-266-head-local-responses-send-body.md)、[B-269](../bugs/B-269-h1-static-files-not-compressed.md)、[F-287](F-287-static-etag-conditional-requests.md)
