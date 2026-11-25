# F-323: HTTP/2 のストリームごとの固定費を減らす（その場で終わる要求はタスク・チャネルを通さずに応答を積む）

**状態: 着手（2026-11-25、feat/httparena-perf。F-317 の子。B-497・F-321 は完了）**

## 今の状態（測った値）

HttpArena の baseline-h2（`h2load -c 256/1024 -m 100 -t 64`、TLS）と baseline-h2c（平文。点には入らない）の 1 要求の CPU（µs）:

| エントリ | baseline-h2-256 | baseline-h2-1024 | baseline-h2c-256 | 使ったコア |
|---|---|---|---|---|
| okena | 1.3（16.77M rps） | 1.3 | 1.2 | 21〜23 |
| agensio | 1.3（14.74M） | 1.3 | 1.1 | 15〜20 |
| h2o | 1.9（16.11M） | 2.0 | — | 30 |
| nginx | 19.0（3.41M） | 19.3 | 26.9 | 64 |
| veil-api | **10.2（6.10M）** | **14.1（4.47M）** | 10.5 | 62〜64 |
| veil（Proxy-Wasm） | 118.7（0.52M） | 108.0 | 106.0 | 61〜63 |

- 上位 3 つは 20〜30 コアしか使っていない（負荷ツール h2load の上限、約 16M rps で止まっている）。veil-api は 64 コアを使い切って 6M。
  1 要求の費用を 4 µs 以下にすれば、veil も負荷ツールの上限に近づける。
- ノート PC（1 ワーカー、io_uring、veil-load の h2c 16 接続 × 100 ストリーム）の veil の `Respond` は 4.5 µs。

## 1 ストリームでしていること（`src/proxy/h2.rs`）

1. HEADERS の受信で `h2_spawn_for_request` → `svc.spawn` → タスクのプールの `spawn_inline_with(h2_request_task)`。応答の
   `stream_channel`（送り口・受け口）、`Notify`、`H2RequestCtx`（`Instant::now()`、`Rc<str>` のクライアントの IP 等）、メインループの
   `H2ActiveStream` の表の項目を作る。
2. タスクの中（`h2_dispatch`）: 疑似ヘッダ以外のヘッダの表を `Vec<(&[u8], &[u8])>` へ集める（確保）、`CURRENT_CONFIG.load()`、
   ルートの解決（F-321 の共有の書き込みを含む）、応答のヘッダを `Vec<(Bytes, Bytes)>` で作り（`h2_base_headers`）、`H2RespMsg` を
   チャネルへ送って `Notify` を鳴らす。`user-agent` の値の複製・アクセスログの判定も要求ごと。
3. メインループの `drive_h2_streams` が全ストリームのチャネルを見て、HPACK で符号化し、HEADERS・DATA を `write_buf` へ積む。
   1 周回の終わりに 1 回だけ送る（ここは既にまとまっている）。

## 改修内容（測ってから、効くものだけ）

1. **その場で終わる要求の近道**: `spawn_inline_with` の最初の poll で応答まで終わった（`Respond`・すぐ返るハンドラ・静的配信の
   キャッシュの当たり・エラー応答）なら、チャネル・`Notify`・`H2ActiveStream` を作らずに、応答をメインループが直接 HPACK で符号化して
   `write_buf` へ積む（タスクの中では応答を小さな値〈状態・ヘッダの表・本文〉としてスロットへ書き、メインループが取り出す）。
   終わらなかった要求だけ今の経路（チャネル）へ移す。
2. **ヘッダの表の確保をやめる**: ルートの解決に渡す要求のヘッダは `HeaderField` の並びを借りて走査する（`Vec` に集めない）。応答の
   ヘッダはワーカーのプールの表を使い回すか、固定の値（`content-type`・`server` 等）は `&'static` の並びで渡す。
3. **HPACK の符号化の費用**: よく出る応答のヘッダ（`:status 200`・`content-type: text/plain` 等）を、接続の動的表に一度入れて
   以後は添字 1 バイトで送る（今の符号化器の索引の使い方を確かめる）。`content-length`・`date` は値だけ符号化する。
4. `Instant::now()`・`user-agent` の複製は、アクセスログ・計測が無効なら取らない。
5. 1〜4 は perf（aofish の perf コンテナ、ノート PC はサイクルの交互 A/B）で上位に出た順に行う。h2load（`-m 100`）と同じ形の負荷は
   veil-load（h2c）と、aofish の HttpArena の h2load の Docker イメージ（TLS）で掛ける。

## 進捗

### 1. ストリームのタスクの future を写さない（2026-11-25）

- aofish の perf（b498、epoll、h2c `Respond` 64 接続 × 100）で、ユーザー時間の 10.5% が `memmove` だった: タスクのプールの
  `write_empty_slot`（5.5%）と `drop_slot`（5.0%）。HTTP/2 のストリームのタスク（`h2_request_task`）の future は 12,416 バイトある。
- `write_empty_slot` は `#[inline(never)]` の `construct` が作った future を戻り値の一時領域から `memcpy` していた。この形は
  「呼び出しの戻り値を直接スロットへ書かせる（call slot optimization）」ためだったが、今の LLVM はスロットが書ける領域だと
  分からないので行わない（`construct` を unwind しない `extern "C"` にしても同じ。小さなクレートで確かめた）。`make()` を
  `write_empty_slot` の中へ展開させると、future の初めの状態（引数 296 バイトと状態の番号）だけをスロットへ書く。
  `h2_request_task` に `#[inline]` を付けて、future を作る関数を展開できるようにした。
- `drop_slot` は `Option::take()` で future 全体を一時領域へ写してから捨てていた（終わったタスクでは中身が `None` でも写す）。
  その場で `None` を書く（中身があればその場で drop）。io_uring・reactor の両方。

- aofish（81aaa3cf、F-317 の「計測の記録」と同じ条件・交互 2 巡。f322 → t1）:

  | 負荷 | epoll rps | epoll 1 要求の CPU | io_uring rps | io_uring 1 要求の CPU |
  |---|---|---|---|---|
  | h2c `Respond`（64 接続 × 100） | 2.72M → 3.20M（+17.3%） | 2.84 → 2.41 µs（−15.5%） | 2.57M → 3.08M（+19.6%） | 2.93 → 2.49 µs（−15.2%） |
  | h2（TLS）`Respond`（64 接続 × 100） | 2.15M → 2.50M（+16.1%） | 3.55 → 3.00 µs（−15.4%） | 2.01M → 2.27M（+13.3%） | 3.79 → 3.34 µs（−11.8%） |

  Wasm の h2c・h2（1 要求 19〜20 µs）は ±3% の中（Wasm の費用が大半）。HTTP/1.1・HTTP/3・静的配信も ±3% の中。
- 1 の後の残り（aofish の perf、t1、epoll、h2c `Respond`）: ヘッダの処理が約 24%（`decode_and_set_headers` 7.9%〈検証・`:path` の
  正規化を含む〉、HPACK の復号 4.9%、`take_request_parts` 2.7%、`recv_headers` 2.0%、`append_header_fragment` 1.7%、
  `FrameDecoder::decode` の中の複写 2.8%〈HEADERS のブロックを `Vec` へ写し、ストリームの `pending_headers` へもう一度写す〉、
  `from_utf8` 1.8%）、`drive_h2_streams` 4.9%、応答のチャネル（`try_recv`）1.9%、`h2_spawn_for_request` 2.1%、arc_swap 約 5.9%
  （設定の Guard を接続の間持ち続けて遅い経路に落ちていた。[B-501](../bugs/B-501-arc-swap-guard-held-across-await.md) で直した）。

### 2. ストリームごとの時刻の読み取りと UTF-8 の検査を減らす（2026-11-25、1b6d2d59）

- io_uring の粗い時計（`coarse_now`）を reactor と同じく実行器の起床ごとに 1 回読む時刻にした。HTTP/2 のストリームの最終活動時刻
  （`Stream::new`・`update_activity`）と 1 秒以上のタイムアウトの期限はこの時刻を使う（ストリームごとの `Instant::now()` が消える）。
- 接続のメトリクスのホスト名は最初の要求のときだけ UTF-8 として読む（h1 と同じ `wants_host`）。`:path` の検査
  （`is_valid_request_target`）を 1 回の走査にし、0x80 以上のバイトがあるときだけ UTF-8 を検査する。
- aofish（f319a=b850a901 → f323m=68a04190、交互 3 巡。f323m は F-319 の最初の `clock` の計り方も含むので、この変更の効果は
  Wasm の無い行で見る）:

  | 負荷 | epoll 1 要求の CPU | io_uring 1 要求の CPU |
  |---|---|---|
  | h2c `Respond`（64 接続 × 100） | 2.42 → 2.19 µs（−9.5%） | 2.37 → 2.28 µs（−3.8%） |
  | 静的配信の h2（TLS） | 19.68 → 18.43 µs（−6.4%） | 19.93 → 19.77 µs（−0.8%） |
  | h1 `Respond` | 11.41 → 11.04 µs（−3.2%） | 10.23 → 10.05 µs（−1.8%） |

- 1・2 を合わせて、aofish の h2c `Respond` の 1 要求の CPU は 2.84 → 2.19 µs（epoll、−23%）・2.93 → 2.28 µs（io_uring、−22%）。
  改修内容の 1（その場で終わる要求の近道）・2（ヘッダの表の確保）・3（HPACK）は未着手（次に perf の上位に出るのはヘッダの処理の
  約 24%）。

## 受け入れ条件

- ノート PC の h2c `Respond`（16 接続 × 100 ストリーム）の 1 要求の CPU が半分以下（4.5 → 2.3 µs 以下）。ストリーミング・gRPC・
  上流への中継・Wasm の経路は悪化しない（±3%）。
- h2spec・HTTP/2 の E2E（io_uring・epoll・kTLS）・gRPC の E2E・`cargo test`。フロー制御・RST_STREAM・GOAWAY・トレーラの試験。

## 期待する効果

- baseline-h2 が veil-api 並み（6.1M）→ 10M なら F-317 の試算で +約 290（S5、nginx を超える段）。veil-api の HTTP/2 の全プロファイル
  （baseline-h2c・json-h2c・gRPC）にも効く。

## 関連

- [F-317](F-317-httparena-infra-score-plan.md)（親）・[B-497](../bugs/B-497-h2-p99-stuck-at-run-length.md)・[F-321](F-321-core-path-per-request-shared-writes.md)・
  F-116・F-158・F-162・F-165・F-298・F-300
