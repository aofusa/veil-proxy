# F-287: 静的配信に `ETag`・`Last-Modified` と条件付き要求（304・`If-Range`）を足す（文書は「ETag が効く」と書いている）

**状態: 完了（v0.10.1）**

## 今の状態

- `type = "File"`・`mode = "memory"` の応答は、HTTP/1.1・HTTP/2・HTTP/3 のどれでも `ETag`・`Last-Modified` を付けず、`If-None-Match`・
  `If-Modified-Since` を見ない（いつも 200 で全体を返す）。`ETag`・304 があるのはプロキシのキャッシュ（`[route.cache] enable_etag`。HTTP/1.1 だけ。
  F-257）と API ライブラリの応答（`library.md`）だけ。
- ライブラリの文書は「the proxy's file backend (sendfile, Range, `ETag`)」（`library-production.md`）・「sendfile, kTLS, Range and ETag work as for the
  proxy」（`library-kit.md`）と書いていて、実装と合わない。
- プロトコル間の差ではない（3 つとも無い）が、[B-266](../bugs/B-266-head-local-responses-send-body.md) と同じ調査で見つけ、文書との食い違いがあるので起票する。

## 改修内容

- 検証子: `Last-Modified` は mtime、`ETag` は nginx と同じ形（`"<mtime の 16 進>-<size の 16 進>"`）。`open_file_cache`・`static_file_cache` が既に持つ
  `stat` の結果から作り、要求ごとに `stat` を足さない。圧縮した表現は弱い ETag か、エンコーディングを ETag に含める（`Vary: Accept-Encoding`）。
  `mode = "memory"` はロード時の内容のハッシュから作る。
- 条件付き要求: `If-None-Match`（弱い比較）を先に、無ければ `If-Modified-Since` を見て 304（本文なし、`ETag`・`Last-Modified`・`Cache-Control` 等は残す）。
  `If-Range` は [F-286](F-286-range-requests-h2-h3-static.md) の Range と組み合わせ、一致しなければ 200 で全体。
- HTTP/1.1・HTTP/2・HTTP/3 で同じ関数を使う。304 の応答は [B-266](../bugs/B-266-head-local-responses-send-body.md) の HEAD と同じく本文を送らない。
- 文書: `routing.md`（File）・`library-kit.md`・`library-production.md`（英日）。実装までは文書の「ETag」を消すか「未対応」と書く。

## 受け入れ条件

- E2E: HTTP/1.1・h2c・HTTP/2・HTTP/3 × File・memory で、2 回目の要求に `If-None-Match`・`If-Modified-Since` を付けると 304、ファイルを書き換えると 200。
  `If-Range` の一致・不一致。
- 交互 A/B: 条件付きでない静的配信の性能が変わらない（3B・54KB、ヘッダが 2 行増える分の差を見る）。

## 修正（v0.10.1）

- 静的配信に `ETag`（ファイルは mtime と長さ、memory は内容のハッシュと長さから作る強い ETag。圧縮した表現は弱い ETag）・`Last-Modified`（memory も読み込み時の mtime）を付け、`If-None-Match`・
  `If-Modified-Since` に 304、`If-Range` が合わなければ全体を返す。全プロトコル同じ（`static_response::plan`）。
- 統合試験 `b266_f286_f287_static_responses_match_across_protocols`、`docs/guide/routing.md`（英日）。

## 関連

- [F-286](F-286-range-requests-h2-h3-static.md)、[B-266](../bugs/B-266-head-local-responses-send-body.md)、[B-269](../bugs/B-269-h1-static-files-not-compressed.md)、
  [F-257](F-257-proxy-cache-h2-h3.md)
