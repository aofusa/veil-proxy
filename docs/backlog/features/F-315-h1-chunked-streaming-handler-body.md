# F-315: HTTP/1.1 の chunked の要求本文もストリーミング受信のハンドラへ届いた順に渡す

**状態: 未着手（2026-11-21 起票。機能 × プロトコルの網羅性の確認、feat/protocol-parity）**

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

## 関連

- [F-266](F-266-multipart-file-upload.md)・[B-372](../bugs/B-372-chunked-framing-unbounded.md)・[B-488](../bugs/B-488-protocol-docs-stale-claims.md)
