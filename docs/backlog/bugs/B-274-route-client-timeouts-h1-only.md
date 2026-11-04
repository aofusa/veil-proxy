# B-274: ルートの `client_body_timeout_secs` が HTTP/2・HTTP/3 で効かず、`client_header_timeout_secs` はどのプロトコルでも使われない

**状態: 修正済み（v0.10.1）**

## 出典

[B-266](B-266-head-local-responses-send-body.md) と同じ調査。`[route.security]` のタイムアウトの設定の参照箇所をプロトコルごとに数えた（コードで確認。実機の再現はまだ）。

## 事象

ガイド（`routing.md` の Timeouts、`migration-nginx.md` は nginx の `client_header_timeout`・`client_body_timeout` と「=」で対応させる）と
`examples/config.toml` は、ルートごとに次の 2 つを設定できると書く。

| 設定 | HTTP/1.1 | HTTP/2 | HTTP/3 |
|---|---|---|---|
| `client_body_timeout_secs`（既定 30） | 使う（`compress`・`https`・`buffered` の要求本文の送り出し） | **使わない** | **使わない** |
| `client_header_timeout_secs`（既定 30） | **使わない** | **使わない** | **使わない** |

- HTTP/2・HTTP/3 の要求本文の受信（`h2_request_body`・`http3_stream` の `recv_body`）には要求単位のタイムアウトが無い。HTTP/2 の接続の
  アイドルタイムアウト（B-119）はストリームが無いときだけ、QUIC のアイドルタイムアウトはパケットが来ないときだけ効く。本文を少しずつ送り続ける
  クライアントは、上流の接続とストリームを長く占有できる（`max_concurrent_streams` までは 1 接続で増やせる）。
- `client_header_timeout_secs` は読み込むだけで参照が無い。ヘッダを受け取る前はルートが決まらないので、ルートの設定としては意味を持てない
  （HTTP/1.1 のヘッダの受信は読み込みごとの固定の `IDLE_TIMEOUT` で打ち切っている）。

## 影響

- P2: HTTP/2・HTTP/3 では要求本文の遅い送信（slow POST）を打ち切れない。設定した値が効かないのに、文書は nginx と同じと書いている。

## 修正方針

- `client_body_timeout_secs`: HTTP/2・HTTP/3 の要求本文の受信で、次の DATA が来るまでの時間（nginx と同じく「2 回の読み込みの間」）に適用し、
  超えたらストリームをリセット（HTTP/2 は `RST_STREAM(CANCEL)`、HTTP/3 は `H3_REQUEST_CANCELLED`）して上流への要求も打ち切る。
  I/O ごとにタイマーを張らない（`runtime::time` の既存の仕組み。AGENTS.md の io_uring のタイマーの規則）。
- `client_header_timeout_secs`: ルートの設定から外して `[server]`（リスナー）の設定にする（HTTP/1.1 のヘッダ・HTTP/2 の HEADERS・HTTP/3 の
  HEADERS の受信に効かせる）か、使わないなら設定と文書から消す。古い設定は警告して無視する。
- 試験: E2E で h2c・HTTP/2・HTTP/3 の本文を途中で止めた要求が、設定した秒数で打ち切られること（HTTP/1.1 は今の試験を残す）。

## 修正（v0.10.1）

- `client_body_timeout_secs` を HTTP/2・HTTP/3 の要求本文の受信にも効かせる（最後に本文を受け取ってから、応答を始めるまで）。
  HTTP/2 は接続の見回りで `RST_STREAM(CANCEL)`、HTTP/3 は 1 秒ごとの見回りで `H3_REQUEST_CANCELLED`。I/O ごとにタイマーを張らない。
  上流への要求も打ち切る（途中までの本文を完結に見せない。[B-277](B-277-mid-upload-reset-completes-upstream.md)）。
- `client_header_timeout_secs` はルートの設定にできないので使わない。互換のため受け付け、書かれていたら起動時に警告する。
- `[http2] stream_idle_timeout_secs` が使われていなかった件は [B-276](B-276-http2-stream-idle-timeout-unused.md)。
- 統合試験 `b274_client_body_timeout_applies_to_h2_and_h3`、文書（`routing.md`・`protocols.md`・`migration-nginx.md`・`performance.md`、英日）。

## 関連

- [B-119](B-119-http2-no-idle-timeout.md)（HTTP/2 の接続のアイドルタイムアウト）、[F-226](../features/F-226-toml-route-timeout-retry-mirror.md)（ルートの要求のタイムアウト）、
  [B-266](B-266-head-local-responses-send-body.md)
