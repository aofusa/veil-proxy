# F-327: Proxy-Wasm の経路の 1 要求の費用を下げ、HttpArena の Proxy-Wasm の版で h2o を超える（親）

**状態: 完了（2026-11-26、feat/proxy-wasm-perf。同日の利用者の指示で実装。試算は SDK を使わないフィルタで約 4,315 点・Rust の SDK のフィルタで約 4,210 点で、h2o〈3,874〉と目安の 4,000 点を超えた〈下の「結果」〉。HttpArena への提出は人の確認の後）**

## 背景

- HttpArena の infrastructure（[F-317](F-317-httparena-infra-score-plan.md)）は、将来 Proxy-Wasm の版（フィルタが `/baseline11`・`/baseline2`・
  `/json/{count}` に答える）で提出する。F-317〜F-326 の後の aofish の計測からの試算は、Proxy-Wasm の版が約 3,580 点（6 位）、
  ハンドラを組み込む版（[F-325](F-325-httparena-veil-compiled-handler-module.md)）が約 4,770 点、h2o が 3,874 点。
- Proxy-Wasm の版の内訳: Wasm を通る 5 プロファイル（baseline 482・limited-conn 550・json-tls 407・baseline-h2 159・baseline-h3 214、
  計 1,812）と、通らない 4 プロファイル（pipelined 127〈`/pipeline` は設定の固定応答〉・static-tls 782・static-h2 593・static-h3 268）。
  h2o を超えるには Wasm を通る 5 プロファイルが約 16% 上がればよい。h2o を超えることを目標にし、4,000 点（約 +23%）を目安にする
  （2026-11-26 に F-332・F-333・F-335 を見送ったので、4,000 点は届かない見通しだった。下の「子チケット」。結果は超えた。下の「結果」）。
- HttpArena の baseline・limited-conn は gcannon が GET・POST（Content-Length）・POST（chunked）の 3 種を回すので、要求の 3 分の 2 が POST。
  POST はヘッダの段・本文の段・後始末の段で 3 回ゲストへ入り、1 要求のユーザー時間の上乗せは GET の約 1.5 倍（下の表）。

## 今の状態（2026-11-26、aofish、io_uring、main 3d8148a0）

1 要求あたりのサーバの CPU（µs）。ネイティブは同じ 3 つの口を Rust のハンドラで答える版（F-325 のエントリ）。記録は
`docs/artifacts/proxy-wasm-perf/profile-2026-11-26.md`。

| 負荷 | Proxy-Wasm rps | user + sys | ネイティブ rps | user + sys |
|---|---|---|---|---|
| h1 `GET /baseline11?a=1&b=1`（512 接続） | 499,679 | 6.58 + 8.75 | 735,941 | 2.87 + 7.55 |
| h1 `POST /baseline11?a=1&b=1`（本文 2 B、Content-Length、2 巡） | 429,243・391,498 | 8.85 + 9.00・9.73 + 9.87 | 705,176・701,220 | 3.13 + 7.68・2.99 + 7.88 |
| json-tls（HTTP/1.1 over TLS、`/json/{count}?m=`） | 227,712 | 19.29 + 14.65 | 491,877 | 7.62 + 8.73 |
| h2（TLS）`/baseline2` | 1,176,759 | 6.11 + 0.56 | 2,441,560 | 2.87 + 0.35 |
| h3 `/baseline2` | 137,761 | 6.53 + 0.56 | 203,942 | 4.25 + 0.50 |

`perf record -e cpu-clock:u` の全シンボルを分類した、Proxy-Wasm の版のユーザー時間の内訳（µs/要求）:

| 分類 | h1 GET | h1 POST | h2 | h3 | json-tls |
|---|---|---|---|---|---|
| veil の Proxy-Wasm の層（`src/wasm/` の engine・context・exports・instance_pool・integration） | 0.98 | 1.59 | 0.84 | 0.59 | 1.68 |
| wasmtime の出入り（ファイバーの作成と破棄・`call_async`・トラップの準備。GET 2 回・POST 3 回） | 0.80 | 1.11 | 0.79 | 0.40 | 1.17 |
| ゲストのコード（フィルタ + proxy-wasm の SDK + dlmalloc） | 0.75 | 1.02 | 0.65 | 0.41 | 3.11 |
| ゲストの `memory.copy` のランタイム呼び出し | 0.08 | 0.11 | 0.08 | 0.06 | 2.94 |
| ホスト関数（wasmtime の包み + `src/wasm/host/`） | 0.39 | 0.60 | 0.30 | 0.20 | 0.66 |
| 付随（確保・`core::fmt`・`from_utf8`・写し等） | 約 0.65 | 約 1.1 | 約 0.45 | 約 0.35 | 約 1.4 |
| ネイティブとのユーザー時間の差 | +3.71 | +5.72 | +3.24 | +2.28 | +11.67 |

- システム時間の差（h1 +1.2・json-tls +5.9 µs）はループバックの性質（遅いサーバほど眠っている負荷ツールを起こす費用を払う。F-295・
  F-319 の 4）で、Wasm が足すシステムコールは無い。ユーザー時間を減らせば縮む。
- ゲストとゲストから呼ぶ関数は wasmtime のファイバーのスタックの上で走り、DWARF の巻き戻しが `wasmtime_fiber_start` で止まる。
  子を含む合計では要求の処理の下に入らないので、自己時間の分類で見る。

## 方針

1. **拡張性を性能より優先する**（2026-11-26 利用者）: Envoy の Proxy-Wasm で動くフィルタ・Rust の Proxy-Wasm の SDK で書いたフィルタが、
   そのまま veil で動くことを重視する。今読めるモジュール・フィルタが今できることを減らさない（veil 本体の機能を減らさない）。
   wasm のランタイム（wasmtime）はセキュリティ上デリケートなので、性能のための手直し・独自の拡張・独自の駆動を入れない。
   HttpArena 向けの最適化は、エントリのフィルタ（拡張の側）で行う。
2. veil の側（どのフィルタにも効くもの）は、ゲストから見える呼び出しの順・引数・値を変えないものを先にする。Proxy-Wasm の ABI の意味を
   変えるもの（[F-329](F-329-wasm-single-fiber-entry-per-request.md) の段階 2 の `proxy_on_log` の時機）は、ガイドに書き、利用者の確認を得てから入れる。
3. 段ごとに aofish で交互 A/B を取り、効いたものだけ残す（F-319 と同じ。命令数ではなくサイクルと rps で判断する）。
4. 試算は F-320 と同じ方法（HttpArena の値に aofish で測った比を掛ける）で段ごとに更新し、目標に届いたらそこで止める。

## 子チケット（この順に進める）

| 順 | チケット | 内容 | 見込み（µs/要求、ユーザー時間） |
|---|---|---|---|
| 0 | [F-328](F-328-wasm-perf-measurement-setup.md) | 計測の準備（HttpArena と同じ GET・POST・chunked の混ぜ方、10 要求ごとの張り直し、分類つきのプロファイル） | — |
| 1 | [F-334](F-334-httparena-filter-and-guide.md) | HttpArena のエントリのフィルタ（拡張の側。veil のコードは変えない）を Wasm 向けに最適化する（bulk-memory 無しのビルド・SDK のままの改善・SDK を使わない版）、フィルタを書く人向けの性能の注意（ガイド） | SDK のまま: json-tls −3.4〜3.9（うち bulk-memory 無し −2.2 は測った値）、GET −0.05〜0.1、POST −0.25〜0.4。SDK を使わない版: h1・h2 さらに −0.35〜0.45 |
| 2 | [F-329](F-329-wasm-single-fiber-entry-per-request.md) | 1 要求のファイバーの出入りを減らす。段階 1: 本文の全量がある POST の作成・ヘッダ・本文を 1 回に（意味の変化なし）。段階 2（要確認）: ローカル応答のログ・削除も同じ 1 回に。先に F-331 の `wants_request_body` の 1 箇所を直す | 段階 1: POST −0.5〜0.7。段階 2: GET −0.5〜0.7、POST −0.5〜0.7 |
| 3 | [F-330](F-330-wasm-request-response-copies.md) | 要求と応答の写しを減らす（ヘッダ・本文・ローカル応答の組み立て） | json-tls −0.5〜1.0、POST −0.5、GET −0.2 |
| 4 | [F-331](F-331-wasm-per-request-bookkeeping.md) | Proxy-Wasm の層の要求ごとの段取りとホスト関数を減らす | 全負荷 −0.3〜0.6 |
| — | [F-332](F-332-wasmtime-config-features.md) | 見送り（拡張性の方針。`gc` を外すと `externref` を使うモジュールが読めなくなる。他の機能は外しても費用が変わらない） | （−0.05〜0.15） |
| — | [F-333](F-333-wasm-bulk-memory-libcalls.md) | 見送り（wasm のランタイムに手を入れない。エントリは F-334 のビルドで足りる） | — |
| — | [F-335](F-335-wasmtime-fiber-entry-cost.md) | 見送り（同上。常駐のファイバー・ワーカーごとのエンジン・性能のための版上げもしない） | — |

- F-334 を先にする: veil のコードと版に依存せず、json-tls の `memory.copy`（2.9 µs）が後の A/B の効きを薄めるのも除ける。veil の側（F-329〜F-331）の
  A/B は F-334 の 1（SDK のまま最適化したフィルタ）で測る（Rust の SDK のフィルタで効くことを見る）。
- 依存の無いもの（F-334、F-329 の段階 1、F-330 の 1、F-331）は、F-329 の段階 2 の利用者の確認を待つ間に進められる（ans-04 の 3-3）。
- 0〜4（SDK のままのフィルタ）の見込みを足すと、1 要求の CPU（user + sys）は GET −8%・POST −13%・json-tls −16%・h2 −18%・h3 −10% 前後で、
  試算は 3,830〜3,880 点（h2o 3,874 と並ぶ）。システム時間はユーザー時間が減ると一緒に縮む（ループバック）ので上振れし得る（〜3,930 点）。
  SDK を使わないフィルタ（F-334 の 2）を使えば +40〜50 点。F-329 の段階 2 を入れない場合は約 −70〜80 点で、h2o には届かない見込み。
  h2o を超えるのは、段階 2 と SDK を使わないフィルタを入れた場合で、余裕は小さい。
- 4,000 点は、wasmtime の側（F-333・F-335）と `gc`（F-332）を見送ったので届かない見通しだった（結果は超えた。下の「結果」）。
- 全部を入れても h1 GET でネイティブより約 1.6 µs/要求 多い見込み（veil の層 0.25・出入り 0.4・ゲスト 0.35・ホスト関数 0.3・付随 0.3）。
- HttpArena のサーバは CPU `0-31,64-95`（32 コア × SMT の 64 スレッド。`scripts/lib/profiles.sh`）。aofish の 8 ワーカー（4 コア × SMT）と同じく
  SMT を分け合い、aofish では SMT の兄弟と分け合うと Proxy-Wasm の版の方が大きく遅くなった（×1.33 とネイティブ ×1.09。
  `docs/artifacts/proxy-wasm-perf/profile-2026-11-26.md`）。命令の量と種類を減らす変更はその分も効くと見る。
- 64 スレッドで aofish より重くなり得るもの（ans-04 の 4-1）: 全ワーカー共有の行への書き込み（ファイバーごとのエンジンの `Arc`。aofish の 2 → 4
  ワーカーでは約 +0.07 µs。複数の CCD をまたぐと跳ね得る）、インスタンスの保持の上限（`instance_pool::per_thread_capacity` は
  `available_parallelism()` で割るので、`total_memories` が 4096 なら 1 スレッド 32 個）。F-329 の段階 2（出入りの回数を減らす）は前者を半分にし、
  ローカル応答で context をすぐ消すので後者の圧力も下げる。F-335 を見送ったので、前者は 1 要求 1 回分残る。
- limited-conn は張り直しの費用が大きく、Wasm の削減の効きが baseline より薄い（試算は F-328 の実測で直す）。

## 結果（2026-11-26）

### 試算（F-325 の 3 と同じ方法。`docs/artifacts/benchmark/httparena-perf/scripts/scen2.py`）

HttpArena の veil-api の値（v0.10.1、2026-11-24）に、aofish で測った「その版 / 同じ木の veil-api の設定」の比をプロファイルごとに掛けた。
前 = e2（main 86297922。3d8148a0 との `src/` の差は B-503 の `proxy/detect.rs` だけ）+ F-320 のフィルタ。後 = f331c（ab4c0ead。以後の
本体の変更は試験・文書と、エラーの経路〈B-504・`proxy_get_property` の確かめる順〉と警告だけ）+ F-334 のフィルタ（ABI = SDK を使わない版・
SDK = Rust の SDK のまま最適化した版。どちらも bulk-memory 無しのビルド）。aofish の f327t（io_uring、3 巡を交互、エラーの出た巡は除く）。

| プロファイル | 前 | 後・ABI | 後・SDK |
|---|---|---|---|
| baseline | 474 | 552 | 514 |
| pipelined | 127 | 127 | 126 |
| limited-conn | 540 | 630 | 586 |
| json-tls | 397 | 883 | 883 |
| static-tls | 872 | 801 | 814 |
| baseline-h2 | 157 | 212 | 190 |
| static-h2 | 599 | 598 | 598 |
| baseline-h3 | 209 | 250 | 236 |
| static-h3 | 262 | 261 | 263 |
| 計（9 エントリ中の順位） | 約 3,640（6 位） | **約 4,315（5 位）** | **約 4,210（5 位）** |

- h2o（3,874）と目安の 4,000 点を、どちらのフィルタでも超えた（上は fit 4,521・nginx 4,779）。前の 3,640 は F-320 の時の 3,580 と同じ
  方法の同じ版で、回の違いの揺れ（static-tls）を含む。
- 分母を前と同じ e2-api にすると、後は ABI 約 4,380・SDK 約 4,270 点（同じ木の分母より、static-tls が高く json-tls が低い）。
- Wasm を通る 5 プロファイルの計は 前 1,777 → 後 2,527（ABI、+42%）・2,409（SDK、+36%）。h2o を超えるのに要った +16% を大きく上回り、
  見込み（「子チケット」の 3,830〜3,880 点）も超えた。同じ回の 1 要求のサーバの CPU（user + sys µs）と rps（中央値）:

  | 負荷（aofish の代わりの負荷） | 前 | 後・ABI | 後・SDK |
  |---|---|---|---|
  | h1 GET（`h1-wasm`。baseline・limited-conn） | 16.87（446k） | 13.68（549k） | 14.67（512k） |
  | json-tls | 36.97（205k） | 18.67（390k） | 19.23（390k） |
  | h2（`h2-wasm`。baseline-h2） | 7.22（1,071k） | 5.40（1,431k） | 6.02（1,280k） |
  | h3（`h3-wasm`。baseline-h3） | 7.80（124k） | 6.61（147k） | 7.02（138k） |

  json-tls は「今の状態」の内訳のゲストの `memory.copy`（2.9 µs）とゲストのコード（3.1 µs）が F-334 のビルドとフィルタで、ヘッダ・本文・
  ローカル応答の写しと TLS の記録が F-330 で減り、ユーザー時間が減った分システム時間（ループバックで相手を起こす費用）も縮んだ。段ごとの
  効きは各子チケットの交互 A/B。
- 同じ木の分母（veil-api）の json-tls は、f327t の回で e2-api の 23.10 µs（326k rps）に対し 27.00 µs（279k、−14.4%）で、その分だけ後の
  json-tls の点が高く出る（e2 を分母にすると 756 点。json-tls だけ e2 を分母にした計は ABI 約 4,190・
  SDK 約 4,080 点で、これでも h2o と 4,000 点を超える）。別に二分した回（`apibis`、3 巡）では `422c4721`（TLS の記録の結合）のビルドから
  1 要求の CPU が +1.9 µs（rps −6〜−9%）で、命令数は −600/要求・サイクル数は +4,750/要求（`istat-api`）。同じバイナリの中で
  記録の積み方を切り替えると、記録 1 つは veil-api の json-tls でだけ 0.62 µs 重く、static-tls（両方の版で 2 µs 以上）と Proxy-Wasm の版の
  json-tls（0.50 µs）では軽い。差の大半はビルドごとのコードの配置（B-339）なので、記録 1 つのままにした
  （[F-330](F-330-wasm-request-response-copies.md) の「同じバイナリの A/B」）。

### Wasm を通らない経路（受け入れ条件の ±2%）

`p0`（47a6e284。veil のコードは main 3d8148a0 と同じ）→ `f331c`（ab4c0ead）。f327n（rps・1 要求の CPU、3 巡）と istat327（同じ 2 つの版の
1 要求のユーザー空間の命令数・サイクル数。`perf stat -e instructions:u,cycles:u`。WSL の `perf_event_paranoid` が 2 なのでカーネルの
イベントは数えられない）。どちらもエラーの出た巡は除く（istat327 の epoll の static-tls の後の版はエラーの無い巡が 1/3 巡。他の回・
io_uring も同じ向き）。

| 負荷 | io_uring 命令 | io_uring サイクル | io_uring rps | epoll 命令 | epoll サイクル | epoll rps |
|---|---|---|---|---|---|---|
| h1-plain | −0.3% | −1.7% | +0.0% | +1.0% | +0.6% | −0.7% |
| pipe-plain | +0.2% | +0.8% | −0.2% | +0.7% | −1.2% | −0.3% |
| h2c-plain | +0.0% | −0.7% | +0.6% | +0.4% | +0.9% | −0.6% |
| h2-plain（TLS） | +0.0% | +1.3% | −0.8% | +0.3% | +1.4% | −0.9% |
| h3-plain | +0.4% | +1.6% | −1.5% | −0.2% | +3.1% | −1.9% |
| static-tls | −3.6% | −5.2% | +8.0% | −3.7% | −5.1% | +10.5% |
| static-h2 | +0.2% | −0.8% | −0.4% | +0.1% | +0.8% | −4.1% |

- 命令数は −0.3〜+1.0%（static-tls は TLS の記録の結合〈422c4721〉で −3.6%）。サイクル数は ±1.7% の中で、epoll の h3-plain の +3.1% は
  命令数 −0.2% のままなのでコードの配置の差（B-339）。f327n で ±2% を外れた rps（io_uring の h2-plain −5.6%・h2c-plain −2.5%、epoll の
  h3-plain −3.8%）は istat327 の回では再現しない（−0.8%・+0.6%・−1.9%）。istat327 の epoll の static-h2 −4.1% も命令数 +0.1%・サイクル数
  +0.8% で、f327n では −0.7%。Wasm を通らない経路に退行は無い。
- veil-api（ネイティブのハンドラ。この表の外）の json-tls の 1 要求の CPU +1.9 µs は、上の「試算」の注のとおりビルドの配置の差が大半で、
  同じバイナリの中では記録 1 つ（今）の方が static-tls を含めて軽い。

### 受け入れ条件の確かめ

- 試算: 上の表。h2o を超えた。
- 拡張性: 今読めるモジュール・フィルタが今できることは減らしていない。Proxy-Wasm の ABI の意味を変えたのは F-329 の段階 2（ローカル応答の
  要求のログ・削除の段を要求の段に続けて呼ぶ時機。呼ぶ順・回数・見える値は変えない。2026-11-26 利用者の決定）だけで、ガイド（英日）に書いた。
  Proxy-Wasm の適合の試験（`tests/proxy_wasm_conformance.rs`）・Proxy-Wasm の E2E（全段・全プロトコル、io_uring と epoll）が通り、試験の
  モジュールは全部読める。wasmtime の cargo の機能・`third_party/wasmtime`・エンジンの設定（`wasmtime::Config`）は変えていない（足した
  `Config::new()` は `metering.rs` の単体の試験の中だけ）。
- `max_execution_time_ms`: 終わらない段は続けて呼ぶ段の中でも打ち切られ、同じスレッドの他の要求を止めない（F-329 の単体の試験）。既存の
  E2E（`test_wasm_timeout`）も通る。
- 試験（最後の版で、手元の 1 回のビルドずつ）: `cargo fmt --check`・`cargo clippy --all-targets -D warnings`（full・full,epoll）、単体 + 統合（full 1,534 + 63 件・
  full,epoll 1,492 + 63 件）、proptest・Proxy-Wasm の適合（20 件）・F-148・取り消しの試験、自己完結型 E2E（full・full,epoll、81 件ずつ）、
  E2E（io_uring・epoll・kTLS、597 件ずつ）、`--no-default-features` と既定のビルド、`wasm`・`wasm,http2`・`wasm,http3`・`wasm,epoll` の
  `cargo check`、`cargo doc`（`-D warnings`）と doc の試験、`check-docs.sh`・`check-messages.py`・`render-defaults.sh --check`・
  `check-guide-configs.py`・`gen_dashboard.py --check`、ライブラリの E2E の `nonregression`。すべて通った。
- 文書: ガイド（英日）の Proxy-Wasm の章に、フィルタを書く人向けの性能の注意（F-334）・ログの段の時機（F-329）・ログの段を呼ぶ範囲（B-504）を
  書いた。設定のキーは足していない（`examples/config.toml` は変えない）。
- 見つけたバグ: [B-504](../bugs/B-504-h1-body-error-skips-wasm-on-log.md)（ログの段が抜ける経路。main から。このブランチで直した）。

### レビュー（Gemini）

計画（req-04）、F-334（req-05）、F-329 の設計と差分（req-06・req-07）、F-330（req-08）、TLS と F-331 の 1（req-09）、F-331 の 2 と F-330 の 3
（req-10）、試験と B-504（req-11）、B-504 の修正（req-12）、その指摘の対応と TLS の記録の判断（req-13）。指摘と扱いは `docs/artifacts/proxy-wasm-perf/gemini/` の各回答と各子チケットの
「実装」に書いた。

### 残したもの

- `proxy_on_log` から応答のステータスとヘッダを見せる（[F-336](F-336-wasm-on-log-response-values.md)）。
- 範囲外の気づき（下）は [F-337](F-337-per-request-security-and-h2-header-vectors.md)・[F-338](F-338-wasm-envoy-local-reply-and-request-time.md) に起票した。
- 要求の future ごと捨てたときは今までどおり次の利用で `proxy_on_delete` だけ（B-177）。
- HttpArena への提出（Proxy-Wasm の版で出すか・いつ出すか）は人の確認の後。フィルタは手元（`docs/artifacts`）にある。

## 分担（Claude Code と Gemini）

- Claude Code: コードの変更・ビルド・試験・aofish の計測・チケットの更新・コミット。
- Gemini（同じ tmux のセッションの別の枠）: 調査・文書の読解・各コミットのコードレビュー（読むだけ。ビルド・試験・git の状態の変更はしない）。
  やり取りは `docs/artifacts/proxy-wasm-perf/gemini/`（依頼 `req-NN-*.md` と回答 `ans-NN-*.md`。決まりは同じ場所の `README.md`）。
- 子チケットごとに、着手の前に Gemini に設計の確認を、コミットの後に差分のレビュー（ホットパスの規則・ABI の意味・`cfg(feature)`・
  `unsafe`・試験の漏れ）を頼み、指摘は直すか理由を書いて退ける。
- 計画のレビュー（`docs/artifacts/proxy-wasm-perf/gemini/ans-04-design-review.md`）の指摘は全部チケットへ入れた（扱いは
  `gemini/notes-04-disposition.md`）。

## 計測（全子チケット共通）

- aofish（サーバは CPU 0-7・ワーカー 8 本、負荷は 8-15）。計測と重いビルドは aofish の計測の鍵を取る。
- 交互 A/B を 2 巡以上（揺れる負荷は 3 巡）。小さな変更は環境変数で切り替える 1 つのビルドの中で比べる（B-339。配置だけで 15% 動く）。
- 負荷: h1 GET・h1 POST（Content-Length・chunked）・HttpArena の混ぜ方（F-328）・10 要求ごとの張り直し・json-tls・h2（TLS）・h2c（参考）・h3。
  Wasm を通らない経路（h1-plain・h2c-plain・静的配信）の退行の確認も毎回。
- veil の側（F-329〜F-331）の A/B は F-334 の 1 のフィルタ（SDK のまま最適化）で測り、既定のビルドのフィルタでも 1 巡確かめる。
- 値は 1 要求あたりのサーバの user + sys（µs）と rps、エラーの数。効果の帰属は分類つきのプロファイル。epoll（reactor）でも退行が無いことを確かめる。

## 受け入れ条件

- Proxy-Wasm の版の試算（F-320 と同じ方法）が h2o（3,874 点）を超える（4,000 点は目安）。届かない場合は、届かない理由と残りの選択肢を親に
  書いて利用者の判断を仰ぐ。
- 今読めるモジュール・フィルタが今できることが減っていない（Proxy-Wasm の適合の試験〈`tests/proxy_wasm_conformance.rs`〉・Proxy-Wasm の E2E が
  通り、試験のモジュール全部が読める）。veil の wasmtime の cargo の機能と設定は変えていない。
- Wasm を通らない経路に退行が無い（±2% の中）。
- Proxy-Wasm の E2E（全段・全プロトコル、io_uring と epoll）、自己完結型 E2E、単体・統合、`cargo clippy`・`cargo fmt`、feature の組み合わせ
  （`--no-default-features`・`wasm` 単体等）。`max_execution_time_ms` で無限ループのフィルタが打ち切られ、他の要求が止まらないこと。
- 挙動・設定を変えたものは同じ変更で `docs/guide/`（英日）と `examples/config.toml` を更新する。

## 範囲外の気づき（Wasm に限らない。F-337・F-338 に起票した）

- `l7::check_security` が要求ごとに `SecurityConfig::ip_filter()` で `IpFilter` を作り直している（空の一覧なら確保は無い。設定のロードの時に
  解決できる）。
- `h2_dispatch` が全要求で経路の解決用の `headers_raw` を `Vec` に集めている（HTTP/1.1 は 16 個までスタックの配列）。
- Envoy との互換性（要確認。拡張性の方針に関わるので、確かめて別のチケットで扱う）:
  - Envoy はローカル応答（フィルタや Envoy 自身が返す 4xx 等）も応答の段（`proxy_on_response_headers` 等）を通すと見ている
    （`FilterManager::sendLocalReplyViaFilterChain` が先頭の encoder フィルタから符号化する）。veil は通さない。
  - `request.time` は veil では呼んだときの時刻（秒の 10 進の文字列。`src/wasm/host/properties.rs`）。Envoy の属性の定義は要求の最初の
    バイトを受けた時刻（値の形式とあわせて確かめる）。

## 関連

- [F-317](F-317-httparena-infra-score-plan.md)・[F-318](F-318-wasm-per-request-shared-writes.md)・[F-319](F-319-wasm-per-request-fixed-cost.md)・
  [F-320](F-320-httparena-veil-entry-config.md)・[F-325](F-325-httparena-veil-compiled-handler-module.md)・[F-326](F-326-reactor-batched-sends.md)
- 調査: `docs/artifacts/proxy-wasm-perf/`（プロファイル、Gemini の回答 `gemini/ans-01-host-path.md` 等）
