# F-330: Proxy-Wasm の経路の要求と応答の写しを減らす（ヘッダ・本文・ローカル応答の組み立て）

**状態: 進行中（[F-327](F-327-proxy-wasm-path-cost.md) の子。1・3 の一部を実装。計測待ち）**

## 現状

`docs/artifacts/proxy-wasm-perf/gemini/ans-01-host-path.md` の段の表（コードで確かめた箇所は下に `path` を書く）:

- ローカル応答（全プロトコル）: `proxy_send_local_response` が本文をゲストの線形メモリから `to_vec()` し、ヘッダを `deserialize_headers`
  （名前と値ごとに `Vec<u8>`）で取り出した後、`sanitize_local_response_headers` で別の `Vec` へ詰め直す（`src/wasm/host/stream.rs`）。
- HTTP/1.1 のローカル応答: `wasm_local_response_h1`（`src/proxy/h1.rs`）が要求ごとに `Vec` を確保し、ステータス行と Content-Length を
  `write_fmt` で書き、本文をもう一度写す。ネイティブのハンドラ（`write_api_response`）はプールのバッファにヘッダを書き、3 KB を超える本文は
  `write_all_vectored` で写さずに送る。json-tls（数〜十数 KB）で本文を 2 回写している。プロファイルの `core::fmt` は h1 で 0.13 µs/要求。
- HTTP/3 のローカル応答: 本文を `Bytes::copy_from_slice(&resp.body)` でもう一度写す（`src/http3_server/request_task.rs` の 6 箇所）。
  HTTP/2 は `Bytes::from(resp.body)` で写さない。
- POST の本文の段（HTTP/1.1）: ヘッダと同じ読み取りで届いた本文を `accumulated.to_vec()` し、`read_full_request_body` で確保し直して写す。
  ヘッダの段で作ったヘッダの写し（`Vec<(Vec<u8>, Vec<u8>)>`）と `Arc<str>` 3 つを本文の段でもう一度作り（`h1.rs`・`h2.rs`）、
  `execute_on_request_body` でさらに `r.headers.to_vec()` する（POST のヘッダの写しは 3 回）。本文の段のローカル応答は `write_or_hold`
  ではなく `write_all`（パイプラインでまとめて送れない。F-322）。
- ヘッダの段のヘッダの写しと `Arc::from` は F-319 で「合わせて 2〜3% で作り直しに見合わない」として見送った。

## 改修内容

1. ローカル応答の組み立て: ヘッダの取り出しと検査（CR・LF・NUL の除去・小文字化）を 1 回の走査にし、HTTP/1.1 はプールのバッファへ
   ヘッダを書いて（数字は `itoa` 等）、大きい本文は `write_all_vectored`・小さい本文は同じバッファへ続けて `write_or_hold` で送る。
   HTTP/3 は `Bytes::from(resp.body)` にする。本文のゲストのメモリからの最初の写し（`src/wasm/host/stream.rs` の `to_vec()`）は残るが、
   これを 1 回だけの写しにする（プロトコルの側で写し直さない。HTTP/1.1 でヘッダと同じバッファへ写す形にするなら最初の写しをそこへ直接）。
2. json-tls の TLS の分類が Proxy-Wasm の版だけ +0.93 µs/要求 重い（`docs/artifacts/proxy-wasm-perf/profile-2026-11-26.md`。暗号化の本体は同じ）。
   ローカル応答を毎回確保する `Vec` に `write_fmt` で書く今の形（`wasm_local_response_h1`）と、ネイティブの `write_api_response`（プールの
   バッファにヘッダを書き、3 KiB 以下の本文は同じバッファへ続けて `write_or_hold`、超える本文は `write_all_vectored`）の違いで説明できるかを
   1 の前後で確かめる。
3. POST: 本文の全量が読み取りバッファにあれば写さずに切り出す（`Bytes`）。[F-329](F-329-wasm-single-fiber-entry-per-request.md) の 3
   （本文の全量があるときヘッダと本文の段を 1 回で呼ぶ）が通る要求は、本文の段のヘッダ・文字列の作り直しがそもそも無くなるので、
   ここで扱うのは本文を分割して受ける要求の側（ヘッダの段のヘッダ・文字列を要求の並び〈`RequestSessions`〉に持たせる等）と、
   本文の段のローカル応答の `write_or_hold`。
4. 1〜3 の後で、ヘッダの段の写し（F-319 で見送ったもの）を測り直し、全段で 1 つの表現（要求の読み取りバッファを指す `Bytes` の組等）に
   できるか判断する。

## 危険

- ローカル応答のヘッダの検査（CR・LF・NUL の除去・空のヘッダ名の拒否・小文字化。B-318）・`Connection: close`（B-499）・HEAD/204/304 の
  本文の抑止（B-188）・Content-Length の補い（F-62）・パイプラインの溜めと送出の順（F-322 の `flush_held`）を保つ。
- `write_all_vectored` は io_uring・epoll・kqueue・WSAPoll の全バックエンドと kTLS・rustls・平文で確かめる。
- 本文の段のローカル応答を `write_or_hold` にするとき、閉じる経路（`client_wants_close`・エラー）で溜めた応答を送ってから閉じる
  （送らずに閉じるとパイプラインの前の応答が消える）。

## 受け入れ条件

- aofish の交互 A/B で json-tls・POST・h3 の 1 要求のユーザー時間が下がる。`alloc-stats` のビルドで 1 要求の確保の回数を前後で記録する。
- 上の危険の各挙動の E2E が通る（既存 + 足りなければ足す）。

## 実装（2026-11-26）

- 1（ヘッダ）: `src/wasm/host/abi.rs` に `visit_headers`（直列化した表の名前と値を写さずに順に渡す。壊れた表は `false`）を足し、
  `deserialize_headers` をその上に作り直した（前の実装と乱数の表・切り詰め・長さの書き換えで同じ結果になることを試験で確かめる）。
  `proxy_send_local_response` は `local_response_headers` で取り出しと検査（CR・LF・NUL・空の名前を捨て、名前を小文字に）を 1 回の
  走査で行う（捨てる組は写さない。壊れた表は今までどおりヘッダ無し）。
- 1（HTTP/1.1）: `write_wasm_local_response_h1`（`src/proxy/h1.rs`）。ステータス行とヘッダは要求バッファのプールのバッファへ `itoa` で
  書き（`write_fmt` と要求ごとの `Vec` を無くす）、3 KiB 以下の本文は同じバッファへ続けて `write_or_hold`（ハンドラの応答と同じ上限。
  `API_INLINE_BODY_MAX`）、超える本文は `resp` から移して `write_all_vectored`（平文は `writev` 1 回、rustls は 2 つを積んで 1 回の
  `flush_tls_writev`）。kTLS の `write_all_vectored` はヘッダと本文を別に送る（送信が 2 回になる）ので、kTLS は今までどおり 1 つの
  バッファにまとめる。ヘッダの段・本文の段のローカル応答、応答本文の段（`buffered.rs`・`forward.rs`）の 4 箇所。応答ヘッダの段
  （B-300、`sendfile.rs` の `write_h1_wasm_local`）はストリームの型を問わないまれな経路なので、同じバイト列を 1 つのバッファで書く
  （`wasm_local_response_h1`）。送るバイト列は今と同じ（ステータス行・ヘッダの順・`Connection: close`・Content-Length の補い・HEAD と
  1xx・204・304 の本文の抑止）。
- 1（HTTP/3）: ローカル応答の本文を `Bytes::from(resp.body)` で移す（6 箇所。ゲストから写した `Vec` は長さと容量が同じなので確保も無い）。
- 3: 本文の段のローカル応答も `write_or_hold`（パイプラインで次の要求が届いていれば溜める。F-322）にし、閉じる前に溜めた応答を送る。
- 試験: `h1.rs` の `wasm_local_response_sends_the_same_bytes_on_every_path`（ソケットの組で、小さな本文・3 KiB ちょうど・超える本文・
  40 KiB の閉じる応答と HEAD・204・本文の無い 404、溜めてから大きな本文の応答で一緒に送る、溜めない書き方）、`status_line_and_supplied_length`、
  `abi.rs` の `deserialize_matches_the_previous_implementation`、`stream.rs` の `local_response_tests`。

## 関連

- [F-327](F-327-proxy-wasm-path-cost.md)（親）・[F-319](F-319-wasm-per-request-fixed-cost.md)・F-202（ハンドラの応答の書き方）・F-322・B-188・B-499・F-62
- `docs/artifacts/proxy-wasm-perf/gemini/ans-01-host-path.md` の候補 4・5(b)(c)(e)
