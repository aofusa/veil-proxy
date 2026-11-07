# F-298: unary gRPC の高速経路

**状態: 完了（feat/v0120-perf、2026-11-07）**

親: [F-295](F-295-v0120-perf-harness.md)。送信は [B-333](../bugs/B-333-h2-trailers-flush-per-stream.md)、テールは [B-332](../bugs/B-332-reactor-hot-connection-starves-others.md)。

## 現状

8 CPU、256 接続 × 100 ストリームで 311k call/s、p99 1.18 s、p99.9 3.1 s（`veil-load` も `h2load` も同じ傾向）。
同じ条件の HTTP/2 の GET は 2.06M req/s なので、gRPC の 1 呼び出しは h2 の 1 要求の 6.6 倍の CPU を使っている。
HttpArena の unary-grpc も 1 呼び出しあたり 約 39 µs（baseline-h2c は約 10 µs）。

## 再現と分解

- `send(2)` が 1 呼び出しあたり 2.0 本（[B-333](../bugs/B-333-h2-trailers-flush-per-stream.md)）。
- プロファイルの上位: memmove 13%、`__rust_alloc` 4.4%、`drive_h2_streams`・`h2_serve_streaming`、
  ストリーム表の SipHash（`hash_one` + `Sip13` 3.6%）、要求本文のチャネル（`request_body_channel`・
  `futures::mpsc`）、`HeaderField` の drop。
- 要求の流れ: HEADERS で本文待ちのままタスクを起動 → DATA を `req_pending` → `stream_channel` → pump →
  `futures::mpsc` → `GrpcFrameDecoder` → `Vec` → `Bytes` → デコード。応答は `Response::stream` → `GrpcBody` →
  `H2RespMsg::Head`/`Body`/`HandlerTrailers` の 3 通。

## 仮説

HEADERS と END_STREAM 付きの DATA はほぼ常に同じ読み取りで届くので、**本文が揃ってからタスクを起動すればチャネルも
ポンプも要らない**。応答も 1 通のメッセージで済む。

## 改修内容

1. **本文を待たない判定**: HEADERS の処理のとき、読み取りバッファの次のフレームが同じストリームの END_STREAM 付き DATA
   なら、ストリーミングで起動せずバッファ経路（`process_frame` が本文を溜めて `body_pending=false`）にする。
2. **unary のデコード**: `Request::body` に全量あるときは `Streaming` を通さず、5 バイトのヘッダを読んで `Bytes::slice` でデコードする。
3. **応答**: `Response` に全量 + トレーラの形を足す（`Response::full_with_trailers`）。HTTP/2 は 1 通のメッセージで
   HEADERS・DATA・トレーラを `write_buf` に積む。HTTP/3・gRPC-Web・圧縮はこれまでの経路に落とす（または同じ形を読む）。
4. **ストリーム表**: `HashMap<u32, _>` の SipHash をストリーム ID 用の軽いハッシュへ。
5. その他、プロファイルの上位から要求ごとの確保を減らす。

## 実装と判断

- 1・2・4 は入れた。1 は「読み取りバッファの次のフレームが同じストリームの END_STREAM 付き DATA で全量が見えている」ときだけ溜める
  経路で起動し、ルートの解決・チャネル 2 本・本文の中継のタスク切り替えが消える（メッシュの経路は従来のまま）。
- **4 の調査で別の無駄が見つかった。** `get_or_create_client_stream` は新しいストリームごとに `active_stream_count()`（表の全走査）を呼び、
  1 要求で SipHash を 3 回走らせていた。100 多重で 1 要求あたり 100 件の走査（O(n²)）。「閉じたストリームも `cleanup_closed` まで
  表に残る」ので、表の長さが上限未満なら数えなくても判定は同じ。ハッシャは `upstream_mux` のものを `http2::stream::StreamIdHasher` へ
  移して共有し、ID はクライアントが選べるので積の上位ビットを下位へ畳み込む（下位ビットのそろった ID で同じ位置を狙わせない）。
- 5 は `h2_dispatch` の設定の読み取りを 1 回に（4 回読んでいた）、`drive_h2_streams` の `done` を接続ごとのバッファに。
- **3（応答を 1 通のメッセージに）は採らない。** 残りのプロファイルは平坦で（`memmove` 12% は呼び元が分散、最大でも 14%）、チャネルの
  `Head`/`Body`/`HandlerTrailers` の 3 通は合わせて数 %。`Response` の公開の形を増やす変更に見合わない。

## 計測

`python3 tools/perf/api_ab.py --scenario grpc`（`veil-load --mode grpc`。256 接続 × 100 ストリーム・64 接続 × 10 ストリーム）。
`h2load` でも確認する（負荷ツールの癖でないことの確認）。

## 受け入れ条件

- スループットが落ちず（目標は数倍）、p99 と p99.9 が大きく下がる（秒単位の外れが無い）。
- ストリーミング RPC・gRPC-Web・圧縮・デッドライン・中断の既存の試験が通る。

## 依存・リスク

- 「次のフレームが揃っているか」は読み取りバッファを覗くだけに留める（フレームを読み進めない）。分割して届いたらこれまでの経路。
- 大きな本文はこれまでどおりストリーミング（判定は 1 フレームの範囲だけ）。
