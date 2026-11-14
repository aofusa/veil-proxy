# F-307: 中継の応答の写しを減らす（HTTP/1.1 の応答のヘッダの蓄積・HTTP/3 の上流の読み取り）

**状態: 完了（feat/beat-nginx、2026-11-14）**

## 背景

コンテナ間の 1 接続の HTTP/1.1 + TLS の中継（wrk `-c1`、54KB）で、veil は nginx に p50 で 10µs ほど遅かった
（lab38: nginx 298µs・veil 308µs）。perf のタイムラインでは差は本文の大きさに比例しており（65 バイトの本文では
veil の方が速い）、要求ごとの写しが効いていた。

- HTTP/1.1（`transfer_response_with_compression`・`transfer_https_response_with_compression`。圧縮・Wasm の応答
  フェーズを持つルートの経路）: 応答のヘッダを溜める 64KB の `Vec` を要求ごとに確保して読み込みを写し、最初の本文を
  さらに `to_vec` で写していた（54KB の応答で 1 要求あたり約 108KB の写し）。
- HTTP/3 の中継（`http3_stream::stream_response`・`read_buffered_response`）: 要求ごとに 32KB の読み取りバッファを
  ゼロで埋め、最初の読み込みを見出しのバッファへ写し、見出しの後ろの本文を `Bytes` へ写し、続きの読み込みも
  1 回ごとに `Vec` へ写していた（54KB の応答で約 86KB の写しと 32KB のゼロ埋め）。溜める経路（`buffering = full`・
  圧縮）は読み込みを `Bytes` へ写してから本文の蓄積へ写していた（2 回）。HTTP/2 の中継は `BytesMut` の
  `split_to().freeze()` で写していない。
- HTTP/3 は 32KB ずつ読むので 54KB の応答は 2 回に分かれて quiche へ渡り、その間にメインループが送出するため、
  1 応答が GSO の束 2 つに分かれていた（負荷側の受信の回数が増える。lab41: 1 要求あたり nginx 1.08〜1.12・
  veil 1.40〜1.43 データグラム）。

## 対応

1. HTTP/1.1: 最初の読み込みのバッファ（プールの 64KB）をそのままヘッダの蓄積にする。圧縮しない応答はヘッダを
   書き換えた後、蓄積の中の本文を `SlicedIoBuf` で `write_all_vectored` へ渡し（写さない）、送った後でプールへ返す。
2. HTTP/3: 読み取りバッファはゼロで埋めない容量だけの `Vec`（64KB）にする（io_uring・reactor の読み込みは容量へ
   書く。TLS の上流の平文の受け渡し `copy_drained` も容量へ書くようにした）。
   - 最初の読み込みはそのまま見出しのバッファにする（写さない）。見出しの後ろの本文は、容量の半分以上あれば
     バッファごと `Bytes` にして渡す（quiche へは `send_body_zc` で写さずに渡る）。少なければ写す（少ない本文で
     大きな容量を ACK まで握らない。握る容量は多くても本文の 2 倍）。
   - 続きの読み込みは、長さの分かる応答では「残りの長さ + 1」の容量で読み、読めた分をそのまま渡す（+1 は宣言長を
     超えて届いたバイトを見つけるため。B-104）。長さの分からない応答も容量の半分以上を読めたら渡す。
   - 溜める経路は読み込みを直接本文の蓄積へ写す（1 回）。
   - 1 回の読み込みを 64KB にする（54KB の応答は多くの場合 1 回で読める）。

## 計測

- HTTP/1.1（lab42、aofish、交互 A/B・各 6 回の中央値）: `h2_1_feat_proxy` の HTTP/1.1 48,239 → 50,360 rps（+4.4%）、
  `h2_1_proxy_compression` の HTTP/1.1 24,925 → 25,470（+2.2%）。1 接続の 54KB（3 回の中央値）: p50 308 → 304µs、
  rps 2,897 → 2,913（nginx 297µs・2,845 rps。p90・p99 は veil が低い）。
- HTTP/3（lab43、aofish、h2load-h3、54KB、各 3 回。交互）: 中継（`h3_proxy`）は rps の中央値 30,968 → 32,468（+4.8%）、
  サーバの CPU は 1 要求あたり 215〜232 → 201〜206µs（−8%、3 回とも）。溜める中継（`h3_proxy_buffering`）は
  29,651 → 32,125（+8.3%）、223〜233 → 205〜208µs（−9%）。nginx の中継は 31,578 rps・200〜226µs で、veil は
  中継 +2.8%・溜める中継 +1.7%（回ごとの揺れ ±8% の内側）。静的配信（`h2_1_feat_http3`）は nginx 41,429・veil 40,620
  （−2%）だが、サーバの CPU は nginx 183〜195µs・veil 143µs（−25%）で、負荷側が律速している。
- 1 要求あたりのデータグラム（負荷側の受信、GRO でまとめた単位）は 64KB の読み込みにしても変わらなかった
  （veil 1.43〜1.51、変更前 1.44〜1.46、nginx 1.11〜1.12）。溜める中継（見出しと本文を一度に渡す）でも 1.40〜1.46
  なので、応答が束に分かれるのは上流の読み取りの区切りではない（応答の最後の短いパケットを GSO の束から外していた。F-309）。

## テスト

- 単体: `http3_stream::tests::stream_response_forwards_body_from_uninitialized_buffers`（1xx の読み飛ばし、1 回で
  届く・分かれて届く・宣言長を超えて届く応答）、`take_into_head_moves_first_read_without_copy`、
  `read_chunks_hand_over_large_reads_and_copy_small_ones`、`tls_backend_read_survives_large_record_followed_by_small_records`
  （容量だけのバッファで読む）。
