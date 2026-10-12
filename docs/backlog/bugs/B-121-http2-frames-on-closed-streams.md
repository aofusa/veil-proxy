# B-121: HTTP/2 で閉じたストリームへの WINDOW_UPDATE・DATA を「idle ストリーム」として接続ごと切っていた

**状態: 完了（feat/f197-f200-impl。F-197 の gRPC 相互運用試験で発見）**

## 事象

tonic のクライアントで veil の gRPC サーバ（F-197）を試験したところ、上限超過のメッセージを送った呼び出しの直後に
同じ接続が `PROTOCOL_ERROR: WINDOW_UPDATE frame on idle stream 5` で切られた（tonic は再接続するので試験は通っていた）。
プロキシの HTTP/2 受付（`src/http2/connection.rs`）全体の問題で、API ライブラリ固有ではない。

## 原因

1. `validate_stream_not_idle` が「ストリーム表に無い」ことを idle と見なしていた。応答を送り終えて表から消えた
   ストリーム（closed）にも、相手の WINDOW_UPDATE・DATA は普通に届く（RFC 9113 §5.1。idle は一度も開いていない ID）。
2. 表に無いストリームへの DATA は接続の受信ウィンドウを消費したまま補充していなかった（取り消し・打ち切りのたびに
   接続のウィンドウが減る）。
3. 要求本文を受信中に応答を送り終えたストリーム（上限超過で早期に応答した等）に RST_STREAM を送っていなかった
   （RFC 9113 §8.1 は RST_STREAM(NO_ERROR) で残りの送信を止めてもらう）。

## 修正

1. idle の判定を「表に無く、かつ（偶数 ID か、クライアント起点の最大 ID より大きい）」にした（RST_STREAM の判定と同じ）。
2. 閉じたストリームへの DATA は接続のクレジットを返し（閾値で WINDOW_UPDATE）、ストリームエラー（STREAM_CLOSED）にする。
3. 多重化メインループで、ストリーミング受信中に応答を終えたストリームへ RST_STREAM(NO_ERROR) を送る。

あわせて、ハンドラのストリーミング応答で「すでに用意できている小さな断片」を 16KB までまとめて 1 つの DATA にした
（1 メッセージ 1 フレームだと、クライアントが取り消した直後に届く大量のフレームを h2 クレートが
`max_local_error_reset_streams` 超過として ENHANCE_YOUR_CALM で接続を切る）。

## 試験

- 単体: `http2::connection::tests::frames_on_closed_streams_are_not_connection_errors`。
- `tests/api/library_e2e.sh grpc`（tonic: message too large → 後続呼び出し、cancel_after_first_response）。
