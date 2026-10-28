# B-246: 上流の応答のフレーミングが RFC 9112 §6.3 と逆（`Content-Length` が `Transfer-Encoding` に優先、複数の `Content-Length` を拒否せず、両ヘッダをそのまま転送）

**状態: 修正済み（v0.10.0）**

## 出典

2026-10-27 のセキュリティレビュー 2 回目の FW-03。aofish で実機再現（`docs/artifacts/security/repro/fw-run.log` の T2）。

## 事象

- `src/http_utils.rs::parse_http_response`（L1133〜L1163）は `Content-Length` と `Transfer-Encoding: chunked` が両方あると **`Content-Length` を優先**して
  本文の長さを決める。RFC 9112 §6.3 は Transfer-Encoding を優先し Content-Length を無視する（要求では拒否してよい）。
- `Content-Length` が複数ある応答は、HTTP/1.1 の経路は **最初** を、HTTP/3 の経路（`src/http3_stream.rs::parse_response_headers` L2032 付近）は **最後** を採る。
  値が異なる複数の `Content-Length` は拒否すべき（要求側 L985〜L1025 は拒否している）。
- 両ヘッダを **そのままクライアントへ転送** する。再現では `Content-Length: 5` + `Transfer-Encoding: chunked` + `5\r\nhello\r\n0\r\n\r\n` がそのまま届いた。
  veil は CL=5 で本文を切るので、chunked の本体が複数の読み込みにまたがると残りが **次の要求の応答** として扱われ、上流接続もプールに戻る（B-244 と同種の取り違え）。

- **追記（2026-10-28、3 回目の SP-01）**: kTLS + splice の経路 `src/proxy/splice.rs::splice_transfer_response_ktls`（L330〜L420）も同じ。ヘッダと一緒に読めた初期本文を
  `Content-Length` を超えていてもそのままクライアントへ書き、`remaining` を飽和減算で 0 にして上流接続を keep-alive のまま戻す。chunked の腕も終端チャンクの後に
  同じ読み取りで届いたバイトを転送する。修正はこの経路にも適用し、単体テストに「初期本文が CL を超える」ケースを含める。

## 影響

- 上流が信頼できない（マルチテナントの上流・侵害された上流・両ヘッダを返すバグのある上流）構成で応答の分割（response smuggling）になる。
  既定の構成では上流は運用者のものなので P2。プロキシとして境界のフレーミングを正規化するのが筋。

## 改修案

- `parse_http_response`: TE が chunked なら CL を無視し、転送時に `Content-Length` を落とす（または 502）。CL が複数で値が異なれば 502。
  HTTP/1.1 と HTTP/3 で 1 つの実装に寄せる。
- 単体テスト: CL+TE → chunked、CL 複数（同値）→ 採用、CL 複数（不一致）→ エラー、`Transfer-Encoding: gzip, chunked` → chunked。

## 修正（v0.10.0）

- `http_utils::FramingHeaders` で Content-Length を全部見る（数字以外・`+5` のような符号付き・値の違う複数は誤り）。Transfer-Encoding があれば Content-Length を使わず（RFC 9112 §6.3）、最後が chunked でなければ接続の終わりで区切る。Transfer-Encoding と Content-Length の併記は応答の分割の試みであり得るので誤り。誤りは 502（HTTP/1.1 の全経路・HTTP/2 で受けた要求・HTTP/3 の上流 `http3_stream::parse_response_headers` が同じ判定を共有）。
- 追記の kTLS + splice の経路（`splice_transfer_response_ktls`）もこの修正で覆う: ヘッダと一緒に送るのは `response_body_start` が決めた本文の分（Content-Length を超える初期本文・終端チャンクの後ろは送らない）だけで、chunked の残りは `feed_counted` の消費分だけを書き、終端の後ろにバイトがあれば上流接続を戻さない。区切りの誤りは送る前に 502。
- 試験: 単体 `response_framing_te_overrides_and_conflicts_are_invalid`・`parse_response_headers_framing_conflicts`、E2E `test_b246_conflicting_upstream_framing_is_rejected`（HTTP/1.1・HTTP/2・HTTP/3、`buffering = full` も。終端チャンクの後ろに偽の応答を付けた上流で本文が `hello` だけになること）。修正前のコードで失敗することを確認。

## 関連

- [B-244](B-244-h1-chunked-timeout-upstream-conn-pooled-mid-response.md)、[B-234](B-234-upstream-response-too-many-headers-waits.md)（上流応答の解析の待ち）
