# F-330: Proxy-Wasm の経路の要求と応答の写しを減らす（ヘッダ・本文・ローカル応答の組み立て）

**状態: 完了（2026-11-26、feat/proxy-wasm-perf。[F-327](F-327-proxy-wasm-path-cost.md) の子。1・3 を実装、2 は TLS の記録の数を直して確かめた、4 は見送り）**

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

## レビュー（Gemini、`docs/artifacts/proxy-wasm-perf/gemini/ans-08-review-F-330.md`）への対応

must-fix は無し（送るバイト列・パイプラインの送出の順・HTTP/3 の移動・`cfg` は正しいと確かめた）。

- 4-1（should-fix）: `pairs_hint` が組の数をデータの長さ / 8 まで信じて確保していた（10 MiB のゲストのメモリで約 60 MiB を確保して
  捨てられる。前の `deserialize_headers` も表の長さの検査を通る組の数ならまとめて確保していた）。組ごとに 10 バイト（サイズテーブル
  8・NUL 2）が収まらない組の数は 0、収まっても 64 までにした。壊れた表のローカル応答のヘッダは確保したものを持ち続けずに空の
  `Vec` を返す。
- 7-1（should-fix）: 長さの桁あふれ・最後の NUL の境界・壊れた組の前で止まること・`pairs_hint` の上限・CR だけ・名前の NUL・
  LF だけの値・ASCII でない名前の試験を足した。
- 2-1（nit）: ヘッダの見積もりを固定の 256 バイトからモジュールのヘッダの長さの上限（`wasm_local_head_bound`）にした（kTLS の
  大きな本文・大きなヘッダの応答でバッファを伸ばし直さない）。
- 7-2（nit）: 小さな応答を 2 つ溜めて 3 つめと送る・溜める指定のまま大きな本文の応答・本文を持たないステータスの大きな本文・
  プールのバッファ（1 KiB・4 KiB）を超えるヘッダの試験を足した。rustls の経路は E2E（json-tls と同じ大きさの応答）で確かめる。

## 確保の回数（2026-11-26、手元、`--features full,alloc-stats`、`veil_alloc_allocs_total` の差 / 要求数）

フィルタは HttpArena の ABI の版。前（`320f2e49`）→ 後（`0c7e174b`）、2 巡とも同じ値。

| 負荷 | 確保 / 要求 | 確保したバイト / 要求 |
|---|---|---|
| h1 GET（`get.raw`） | 17.0 → 16.0 | 3,790 → 3,697 |
| h1 POST（Content-Length） | 26.0 → 25.0 | 4,096 → 3,998 |
| h1 POST（chunked） | 27.0 → 26.0 | 4,133 → 4,033 |
| h1 `/json/50`（8.4 KB の本文） | 17.0 → 16.0 | 20,520 → 12,052 |
| h2c `/baseline2` | 21.07 → 21.07 | 4,841 → 4,843 |

HTTP/2 は前から本文を写していない（ヘッダの取り出しと検査を 1 回にしても、前の `collect` は同じ確保を使い回していたので回数は
同じ）。HTTP/3 は応答ごとに本文の写しが 1 つ減る（手元で測っていない。ゲストから写した `Vec` は長さと容量が同じなので `Bytes::from`
は確保しない）。

## TLS の記録（2026-11-26）

1 の後の交互 A/B（`f329` → `f330`）で json-tls の 1 要求の CPU が増えた（io_uring +0.52 µs、epoll +0.73 µs）。3 KiB を超えるローカル応答を
`write_all_vectored` にしたところ、rustls の経路はヘッダと本文を別々に `writer().write` していて TLS の記録が 2 つになっていた（前は 1 つの
`Vec` で 1 つの記録）。`tls_writev::write_plaintext_pair` で 2 つを 1 回の `write_vectored` で積む（rustls 0.23 は要素をまたいで 16 KiB ごとに
記録に分ける。鍵の更新・記録の上限・送信のバッファの上限は `write` と同じ経路）。`simple_tls.rs`・`ktls_rustls.rs` の `write_all_vectored`
（rustls）を使う他の経路（ハンドラの応答・静的配信・キャッシュ・圧縮の応答・chunked の中継・WebSocket）も記録が 1 つ減る。kTLS・平文は
変えていない。試験は `tls_writev.rs` の `plaintext_pair_tests`（1 つの記録・16 KiB を超える組・一部の受理・0 受理からの再開・境目）と統合
試験の `f150_tls_writev_roundtrip`（実際のソケットで 54 KB・3 B・0 B）。

## 交互 A/B（2026-11-26、aofish、`~/hp/bench.py`、3 巡の中央値、1 要求のユーザー + システムの CPU µs）

| 負荷 | io_uring・ABI の版 `f329` → `f330` → `f330b` | io_uring・SDK の版 `f329` → `f330` → `f330b` | epoll・ABI の版 `f329` → `f330` → `f330b` |
|---|---|---|---|
| h1 GET（`h1-wasm`） | 14.11 → 14.14 / 14.04 → 14.14 | 15.28 → 14.88 / 14.48 → 14.43 | 21.22 → 21.18 / 20.31 → 19.51 |
| h1 POST | 16.20 → 15.97 / 15.87 → 15.73 | 17.06 → 16.63 / 16.88 → 16.26 | 24.92 → 25.42 / 24.17 → 23.19 |
| json-tls | 18.75 → 19.27 / 18.58 → 18.62 | 20.08 → 20.26 / 19.19 → 19.02 | 18.60 → 19.33 / 17.75 → 17.65 |
| h2c | 5.65 → 5.77 | 6.38 → 6.38 | 5.69 → 5.75 |
| h3 | 6.99 → 6.84 | 7.38 → 7.27 | 6.56 → 6.61 |

`f330` は `0c7e174b`（1・3）、`f330b` は `422c4721`（TLS の記録）。`f329 → f330b` は別の回（同じ回の中だけで比べる）。json-tls は記録を 1 つに
して `f329` と同じ（増えた分が消えた）。POST は −0.14〜−0.98 µs。HTTP/1.1 の GET は io_uring で ±0.1 µs（誤差の中）、epoll で −0.8 µs。
h3 は本文の写しが減って −0.1〜−0.15 µs。2 の「json-tls の TLS の分類が Proxy-Wasm の版だけ +0.93 µs」は、前の Proxy-Wasm の経路が 1 つの
記録だった（ネイティブのハンドラの `write_all_vectored` が 2 つ）ので、記録の数では説明できない（ネイティブのハンドラの側が記録 1 つ分
軽くなった）。

### 同じバイナリの A/B（2026-11-26、[F-327](F-327-proxy-wasm-path-cost.md) の確かめ）

F-327 の確かめで、veil-api（ネイティブのハンドラ）の json-tls が `422c4721` のビルドから遅くなっていた（aofish の二分〈`apibis`、epoll の
veil-api、3 巡〉で 1 要求の CPU 22.2 µs 前後 → 24.1 µs、rps −6〜−9%。ユーザー空間の命令数は −600/要求、サイクル数は +4,750/要求〈IPC
1.80 → 1.60〉。`istat-api`）。命令が減ってサイクルが増えるので、`write_plaintext_pair` の積み方を環境変数で切り替える一時的なビルド
（B-339 の方法。`docs/artifacts/proxy-wasm-perf/tools/pairmode.patch`）で、同じバイナリの中で比べた（aofish、5 巡の中央値、1 要求の
CPU µs。wrk の時計の戻りでエラーの出た巡〈B-357〉は除く。`pm`）。

| 負荷 | 記録 1 つ（`write_vectored`。今） | 記録 2 つ（2 回の `write`。前） | 1 つのバッファへ写して `write` |
|---|---|---|---|
| veil-api（epoll）の json-tls | 22.79 | 22.17（rps +4.7%） | 23.03 |
| veil-api（epoll）の static-tls | 29.29 | 31.55（rps −13.4%） | 29.62 |
| Proxy-Wasm（io_uring・SDK を使わないフィルタ）の json-tls | 17.60 | 18.10（rps −3.7%） | 17.61 |
| Proxy-Wasm の static-tls | 27.54 | 29.86（rps −12.2%） | 30.66 |

記録 1 つのままにする。記録 2 つが軽いのは veil-api の json-tls（−0.62 µs）だけで、static-tls はどちらの版でも 2 µs 以上重く、Proxy-Wasm
の版（HttpArena へ出す方）の json-tls も記録 1 つが軽い。写してからまとめても軽くならない。1 要求のユーザー空間の命令数・サイクル数
（`istat-pm`、3 巡）も同じ向きで、記録 2 つは命令数がどれも増え（json-tls +540〜590、static-tls +1,300）、サイクル数が減るのは veil-api の
json-tls（−970、−2.2%）だけ。二分のサイクル数の差（+4,750）のうち記録の積み方の分は約 1,000 で、残りはビルドごとのコードの配置の差
（B-339。同じバイナリの記録 1 つは 1 要求の CPU 22.79 µs）。

## 3（追加、2026-11-26）: 本文の段のヘッダの写し

ヘッダと本文の段を続けて呼ぶ経路（[F-329](F-329-wasm-single-fiber-entry-per-request.md)）で、本文の段の context に要求ヘッダの表の写しを
置いていた（本文の段のヘッダの書き換えは上流へ届かない決まりのため。名前と値ごとの `Vec` で、ヘッダ N 個で 2N + 1 回の確保）。表は写さずに
置き、ゲストが本文の段で最初に書き換える前の表だけを取っておく（`HttpContext::request_headers_mut`・`replace_request_headers`。表の置き換えは
前の表を写さずに移す）。上流へ送る表は段の後で context から取り出す（`take_upstream_request_headers`）。HttpArena のフィルタのように本文の段で
ローカル応答を返す要求は、表を作り直さない。試験は `f318_recycle_tests` の `body_phase_headers_are_copied_only_when_written` と、既存の
`body_phase_rewrites_the_body_but_not_the_upstream_headers`（段を続けて呼ぶ経路・別に呼ぶ経路の両方で、足したヘッダは上流へ届かない）。

確保の回数（手元、`alloc-stats`、ヘッダ 2 つの要求）: h1 POST 19 → 14、chunked 20 → 15（[F-331](F-331-wasm-per-request-bookkeeping.md) の 1 回目の後から）。

## 4 の判断

ヘッダの段の写し（`Vec<(Vec<u8>, Vec<u8>)>` と、ゲストに見せる文字列）を要求の読み取りバッファを指す `Bytes` の組に替える案は採らない。
`HttpContext::request_headers` はホスト関数・gRPC・メッシュのデータプレーン・試験が直接読み書きする公開の表で、表現を替えると書き換えの
経路（足す・置き換える・消す・疑似ヘッダ）のすべてに写しの扱いが要る。文字列は F-331 でスレッドの持ち分から複製するようにした。残る写しは
ヘッダ 1 組ごとの 2 回の確保で、HttpArena の要求（ヘッダ 2〜4 個）では 1 要求 5〜9 回。F-319 の判断（作り直しに見合わない）を保つ。

## 関連

- [F-327](F-327-proxy-wasm-path-cost.md)（親）・[F-319](F-319-wasm-per-request-fixed-cost.md)・F-202（ハンドラの応答の書き方）・F-322・B-188・B-499・F-62
- `docs/artifacts/proxy-wasm-perf/gemini/ans-01-host-path.md` の候補 4・5(b)(c)(e)
