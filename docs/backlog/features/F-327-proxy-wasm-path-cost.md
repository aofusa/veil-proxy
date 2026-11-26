# F-327: Proxy-Wasm の経路の 1 要求の費用を下げ、HttpArena の Proxy-Wasm の版で h2o を超える（親）

**状態: 未着手（2026-11-26、feat/proxy-wasm-perf。調査と設計まで。実装は利用者の確認の後。HttpArena への提出は人の確認の後）**

## 背景

- HttpArena の infrastructure（[F-317](F-317-httparena-infra-score-plan.md)）は、将来 Proxy-Wasm の版（フィルタが `/baseline11`・`/baseline2`・
  `/json/{count}` に答える）で提出する。F-317〜F-326 の後の aofish の計測からの試算は、Proxy-Wasm の版が約 3,580 点（6 位）、
  ハンドラを組み込む版（[F-325](F-325-httparena-veil-compiled-handler-module.md)）が約 4,770 点、h2o が 3,874 点。
- Proxy-Wasm の版の内訳: Wasm を通る 5 プロファイル（baseline 482・limited-conn 550・json-tls 407・baseline-h2 159・baseline-h3 214、
  計 1,812）と、通らない 4 プロファイル（pipelined 127〈`/pipeline` は設定の固定応答〉・static-tls 782・static-h2 593・static-h3 268）。
  h2o を超えるには Wasm を通る 5 プロファイルが約 16% 上がればよい。余裕を持って 4,000 点を目標にする（約 +23%）。
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

1. veil の側（どのフィルタにも効くもの）を先にする。wasmtime の手直しは最後に、利用者の判断を得てから。
2. 段ごとに aofish で交互 A/B を取り、効いたものだけ残す（F-319 と同じ。命令数ではなくサイクルと rps で判断する）。
3. 試算は F-320 と同じ方法（HttpArena の値に aofish で測った比を掛ける）で段ごとに更新し、目標に届いたらそこで止める。
4. Proxy-Wasm の ABI の意味を変えるもの（[F-329](F-329-wasm-single-fiber-entry-per-request.md) の `proxy_on_log` の時機）は、ガイドに書き、
   利用者の確認を得てから入れる。

## 子チケット（この順に進める）

| 順 | チケット | 内容 | 見込み（µs/要求、ユーザー時間） |
|---|---|---|---|
| 0 | [F-328](F-328-wasm-perf-measurement-setup.md) | 計測の準備（HttpArena と同じ GET・POST・chunked の混ぜ方、10 要求ごとの張り直し、分類つきのプロファイル） | — |
| 1 | [F-329](F-329-wasm-single-fiber-entry-per-request.md) | ローカル応答の要求を 1 回のファイバーの出入りで終える（GET 2 → 1 回、POST 3 → 1 回）。先に F-331 の `wants_request_body` の 1 箇所を直す | GET −0.5〜0.7、POST −1.0〜1.5 |
| 2 | [F-330](F-330-wasm-request-response-copies.md) | 要求と応答の写しを減らす（ヘッダ・本文・ローカル応答の組み立て） | json-tls −0.5〜1.0、POST −0.5、GET −0.2 |
| 3 | [F-331](F-331-wasm-per-request-bookkeeping.md) | Proxy-Wasm の層の要求ごとの段取りとホスト関数を減らす | 全負荷 −0.3〜0.6 |
| 4 | [F-332](F-332-wasmtime-config-features.md) | wasmtime の設定と cargo の機能を見直す（`gc` 等を外す） | −0.05〜0.15（測って決める） |
| 5 | [F-334](F-334-httparena-filter-and-guide.md) | HttpArena のフィルタを Wasm 向けに書く（bulk-memory 無しのビルドを含む。エントリの側）、フィルタを書く人向けの性能の注意（ガイド） | json-tls −3.4〜3.9（うち bulk-memory 無し −2.2 は測った値）、GET −0.05〜0.1、POST −0.25〜0.4（SDK を外せば h1・h2 −0.35〜0.45） |
| 6 | [F-333](F-333-wasm-bulk-memory-libcalls.md) | ゲストの小さな `memory.copy`・`memory.fill` を wasmtime の側で速くする（要判断。どのフィルタにも効く。エントリは 5 で足りる） | 既定のビルドのフィルタで json-tls −1.0〜2.6 |
| 7 | [F-335](F-335-wasmtime-fiber-entry-cost.md) | ファイバーの出入りの固定費を wasmtime の側で減らす（要判断。手直し・常駐のファイバー・ワーカーごとのエンジン・版上げ） | 1 回 −0.1〜0.35、共有の書き込みが消える |

- 依存の無いもの（F-330 の 1、F-331、F-332 の機能の削減）は、F-329 の利用者の確認を待つ間に先に進められる（ans-04 の 3-3）。
- 1〜5 の見込みを足すと、1 要求の CPU（user + sys）は GET −9%・POST −14%・json-tls −16%・h2 −20%・h3 −11% 前後で、試算は 3,850〜3,900 点
  （h2o 3,874 と並ぶ）。システム時間はユーザー時間が減ると一緒に縮む（ループバック）ので上振れし得る（〜3,950 点）。4,000 点に届くかは
  実測と、7・SDK を使わないフィルタ（F-334 の 2）次第。段ごとに測った値で試算を更新し、届かない見通しになった時点で利用者に諮る。
- 全部を入れても h1 GET でネイティブより約 1.3 µs/要求 多い見込み（veil の層 0.25・出入り 0.1〜0.4・ゲスト 0.35・ホスト関数 0.25・付随 0.3）。
- HttpArena のサーバは CPU `0-31,64-95`（32 コア × SMT の 64 スレッド。`scripts/lib/profiles.sh`）。aofish の 8 ワーカー（4 コア × SMT）と同じく
  SMT を分け合い、aofish では SMT の兄弟と分け合うと Proxy-Wasm の版の方が大きく遅くなった（×1.33 とネイティブ ×1.09。
  `docs/artifacts/proxy-wasm-perf/profile-2026-11-26.md`）。命令の量と種類を減らす変更はその分も効くと見る。
- 64 スレッドで aofish より重くなり得るもの（ans-04 の 4-1）: 全ワーカー共有の行への書き込み（ファイバーごとのエンジンの `Arc`。aofish の 2 → 4
  ワーカーでは約 +0.07 µs。複数の CCD をまたぐと跳ね得る）、インスタンスの保持の上限（`instance_pool::per_thread_capacity` は
  `available_parallelism()` で割るので、`total_memories` が 4096 なら 1 スレッド 32 個）。1（出入りの回数を減らす）は前者を半分にし、
  ローカル応答で context をすぐ消すので後者の圧力も下げる。
- limited-conn は張り直しの費用が大きく、Wasm の削減の効きが baseline より薄い（試算は F-328 の実測で直す）。

## 分担（Claude Code と Gemini）

- 計画のレビュー（`docs/artifacts/proxy-wasm-perf/gemini/ans-04-design-review.md`）の指摘は全部チケットへ入れた（扱いは
  `gemini/notes-04-disposition.md`）。

- Claude Code: コードの変更・ビルド・試験・aofish の計測・チケットの更新・コミット。
- Gemini（同じ tmux のセッションの別の枠）: 調査・文書の読解・各コミットのコードレビュー（読むだけ。ビルド・試験・git の状態の変更はしない）。
  やり取りは `docs/artifacts/proxy-wasm-perf/gemini/`（依頼 `req-NN-*.md` と回答 `ans-NN-*.md`。決まりは同じ場所の `README.md`）。
- 子チケットごとに、着手の前に Gemini に設計の確認を、コミットの後に差分のレビュー（ホットパスの規則・ABI の意味・`cfg(feature)`・
  `unsafe`・試験の漏れ）を頼み、指摘は直すか理由を書いて退ける。

## 計測（全子チケット共通）

- aofish（サーバは CPU 0-7・ワーカー 8 本、負荷は 8-15）。計測と重いビルドは aofish の計測の鍵を取る。
- 交互 A/B を 2 巡以上（揺れる負荷は 3 巡）。小さな変更は環境変数で切り替える 1 つのビルドの中で比べる（B-339。配置だけで 15% 動く）。
- 負荷: h1 GET・h1 POST（Content-Length・chunked）・HttpArena の混ぜ方（F-328）・10 要求ごとの張り直し・json-tls・h2（TLS）・h2c（参考）・h3。
  Wasm を通らない経路（h1-plain・h2c-plain・静的配信）の退行の確認も毎回。
- 値は 1 要求あたりのサーバの user + sys（µs）と rps、エラーの数。効果の帰属は分類つきのプロファイル。epoll（reactor）でも退行が無いことを確かめる。

## 受け入れ条件

- Proxy-Wasm の版の試算（F-320 と同じ方法）が 4,000 点以上。届かない場合は、届かない理由と残りの選択肢を親に書いて利用者の判断を仰ぐ。
- Wasm を通らない経路に退行が無い（±2% の中）。
- Proxy-Wasm の E2E（全段・全プロトコル、io_uring と epoll）、自己完結型 E2E、単体・統合、`cargo clippy`・`cargo fmt`、feature の組み合わせ
  （`--no-default-features`・`wasm` 単体等）。`max_execution_time_ms` で無限ループのフィルタが打ち切られ、他の要求が止まらないこと。
- 挙動・設定を変えたものは同じ変更で `docs/guide/`（英日）と `examples/config.toml` を更新する。

## 範囲外の気づき（Wasm に限らない。別のチケットで扱う）

- `l7::check_security` が要求ごとに `SecurityConfig::ip_filter()` で `IpFilter` を作り直している（空の一覧なら確保は無い。設定のロードの時に
  解決できる）。
- `h2_dispatch` が全要求で経路の解決用の `headers_raw` を `Vec` に集めている（HTTP/1.1 は 16 個までスタックの配列）。

## 関連

- [F-317](F-317-httparena-infra-score-plan.md)・[F-318](F-318-wasm-per-request-shared-writes.md)・[F-319](F-319-wasm-per-request-fixed-cost.md)・
  [F-320](F-320-httparena-veil-entry-config.md)・[F-325](F-325-httparena-veil-compiled-handler-module.md)・[F-326](F-326-reactor-batched-sends.md)
- 調査: `docs/artifacts/proxy-wasm-perf/`（プロファイル、Gemini の回答 `gemini/ans-01-host-path.md` 等）
