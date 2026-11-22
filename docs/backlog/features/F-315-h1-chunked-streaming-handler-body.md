# F-315: HTTP/1.1 の chunked の要求本文もストリーミング受信のハンドラへ届いた順に渡す

**状態: 完了（2026-11-22、feat/protocol-parity。2026-11-21 起票。機能 × プロトコルの網羅性の確認）**

## 今の状態

- ストリーミング受信のハンドラ（`streaming = true`・`App::route_fn_streaming`・`Multipart`）は、HTTP/2・HTTP/3 では本文を
  届いた順に渡し、HTTP/1.1 でも `Content-Length` の本文は届いた順に渡す（F-266）。
- HTTP/1.1 の chunked の本文だけは全部を読んでから 1 つの断片で渡す（`proxy/h1.rs` の `handle_api_handler`）。
  `curl -T -`・ブラウザの外のクライアントの大きなアップロードで、ハンドラが本文を読み始められず全部がメモリに載る。

## 改修内容

- `pump_api_body` を chunked でも動くようにする（チャンクの枠を解きながら本文をチャネルへ送る。終端のチャンクとトレーラを
  読み終えたら終わり。枠が壊れていたら中断＝`abort`）。`read_full_request_body` の chunked の検査（大きさの行・拡張・
  トレーラの上限。B-372）と同じ規則を使う。
- 本文の後ろの次の要求（パイプライン）は `overflow` へ返す（`Content-Length` と同じ）。

## 受け入れ条件

- 単体: chunked の本文が断片ごとにハンドラへ届く（最初の断片を読んだ時点でハンドラが進む）。壊れた枠・途中の切断は
  `take_body_stream` に中断として見える。パイプラインの次の要求が処理される。
- 文書（library、英日）。

## 対応

- `proxy/h1.rs` に `pump_api_chunked_body` を足した。見出しと一緒に届いた分（`initial_body`。終端があれば呼び出し側が終端の直後で
  切っている）から読み始め、読み取りごとに `next_data_span` でチャンクの枠を解いた本文を 1 つの断片にまとめてチャネルへ送る
  （`Content-Length` の `pump_api_body` と同じ単位。確保は読み取りごとに 1 回）。枠の検査は本文を溜める経路と同じ
  `ChunkedDecoder::new_request(0)`（大きさの行・拡張・トレーラの上限は B-372 のまま、全体の大きさは見ない＝ストリーミング受信の
  `Content-Length`・HTTP/2・HTTP/3 と同じ）。終端のチャンクとトレーラまで読めたら正常な終わり、枠が壊れた・終端の前に切れた・
  読み取りが止まったら `abort`（`is_aborted`）。終端より後ろまで読んだ分は `overflow`（次の要求。B-186）へ足す。
- `handle_api_handler` の分岐を `streaming_request && (is_chunked || content_length > 0)` にし、`handle_api_streaming` に `is_chunked`
  を渡す（後始末は本文を読み終えたかどうかだけで決まるので共通）。
- chunked の本文を自分で読むハンドラは、`Expect: 100-continue` に 100 Continue を返してから読む（`Content-Length` の要求は要求の
  処理のループが返している。以前は chunked のハンドラの要求に 100 を返さず、curl は 1 秒待ってから送っていた。溜める経路の
  chunked も同じ）。Wasm の本文の段（B-189）は本文を Content-Length に詰め直すので重ならない。
- 文書（library、英日）と rustdoc（`Request::take_body_stream`・`api::stream`）。

### 試験

- 単体 `proxy::h1::b186_overflow_tests::pump_api_chunked_body_streams_pieces_in_arrival_order`: 見出しと一緒に届いた分が、残りを
  送る前にハンドラへ届く。チャンク拡張・トレーラを通り、終端の後ろの次の要求は `overflow` へ戻る。
- 単体 `pump_api_chunked_body_aborts_on_broken_or_cut_body`: 16 進でない大きさの行・終端の前の切断は `is_aborted`。
- ライブラリの E2E（`tests/api/library_e2e.sh rust`）: `/upper` へ chunked で送る（curl の `-H 'Transfer-Encoding: chunked'`、
  1 MiB の `curl -T -`＝`Expect: 100-continue` 付き）、`Expect: 100-continue` で 100 を受けてから 2 回に分けて送る chunked の本文と
  同じ書き込みの次の要求（パイプライン）。
- clippy（`full`・`full,api`）。

## 関連

- [F-266](F-266-multipart-file-upload.md)・[B-372](../bugs/B-372-chunked-framing-unbounded.md)・[B-488](../bugs/B-488-protocol-docs-stale-claims.md)
