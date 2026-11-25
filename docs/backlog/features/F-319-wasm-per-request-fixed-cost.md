# F-319: Proxy-Wasm の 1 要求あたりの固定費を減らす（ヘッダの写し・確保・コールバックの回数・fuel）

**状態: 完了（2026-11-26、feat/httparena-perf。改修内容の 3 の一部と 4・5 を行い、1・2 と 3 の `phases` は測って見送った。epoll の上限の負荷での差は送出の束ね方の問題で、[F-326](F-326-reactor-batched-sends.md) に分けた）**

## 今の状態

ノート PC（4 CPU、veil-load、`docs/artifacts/benchmark/httparena-perf/results/m1.tsv`）の 1 要求あたりの CPU（µs）:

| 経路 | v0.10.1 | main |
|---|---|---|
| h1 Wasm 無し（`/plain3`） | 19.3 | 12.3 |
| h1 Wasm の GET（`/baseline11`） | 40.4 | 37.1 |
| h1 Wasm の POST（本文 20 B） | 55.8 | 49.8 |
| h1 Wasm の `/json/{n}` | 58.7 | 52.7 |
| h2c Wasm 無し | 8.4 | 4.5 |
| h2c Wasm の GET（`/baseline2`） | 23.3 | 29.5 |

- Wasm の上乗せは h1 で 約 25 µs、POST は本文の段の分さらに 約 13 µs。nginx の C のモジュールとの差の大半。
- **h2c の Wasm だけ main で遅くなっている**（上乗せ 14.9 → 25.0 µs。h1 は 21.1 → 24.8 µs）。共有の機械で揺れがあるので、まず
  交互 A/B で確かめる（v0.10.1 以降に Wasm の HTTP/2 の経路へ入った変更: B-408 の `set_request_info`〈`:authority` の `Arc::from`〉等）。

要求ごとの主な費用（`proxy/h1.rs`・`proxy/h2.rs`・HTTP/3・`wasm/engine.rs`）:

1. ヘッダの写し: `Vec<(Vec<u8>, Vec<u8>)>` を要求ごとに作る（ヘッダの数 × 2 回の確保）。ゲストへ渡すのは `proxy_get_header_map_pairs`
   の直列化のときだけで、多くのフィルタは数個のヘッダしか見ない。
2. 文字列の確保: `Arc::from(path)`・`Arc::from(method)`・`Arc::from(client_ip)`・`Arc::from(authority)`。
3. コールバックの回数: 1 要求で `proxy_on_context_create` → `proxy_on_request_headers` →（POST）`proxy_on_request_body` → 応答の後に
   `proxy_on_log` → `proxy_on_done`・`proxy_on_delete`。それぞれ `call_async`（ファイバーへの出入り）と `arm_store_until`
   （`set_fuel`・`fuel_async_yield_interval`・`set_epoch_deadline`）。フィルタが `send_local_response` で応答したあとの
   `on_request_complete_async` もファイバーを通る。proxy-wasm の Rust SDK は全コールバックをエクスポートするので、
   「エクスポートが無ければ呼ばない」は SDK のフィルタには効かない。
4. fuel: `consume_fuel(true)` はゲストの全関数の基本ブロックに残量の検査を入れる（ゲストの実行が遅くなる）。打ち切り
   （`max_execution_time_ms`）と協調的な yield（`yield_interval_us`）の両方に使っている。エポック（`epoch_interruption(true)`、
   100 ms ごとの専用スレッド）は既にある。
5. 呼び出しのフューチャの `Box::pin`（`on_request_headers_with_modules_async`）。

## 改修内容（測ってから、効くものだけ）

1. ヘッダは要求のバッファ（`Bytes`）を借りた表を `HttpContext` に置き、`proxy_get_header_map_pairs`・`proxy_get_header_map_value` で
   ゲストのメモリへ直接書く（写しは変更されたときだけ作る。`proxy_add/replace/remove_header_map_value` で初めて所有の表にする）。
2. path・method・authority・client_ip は `Bytes` のスライスか、ワーカーの作業領域に置く（`Arc::from` をやめる）。
3. コールバックを減らす:
   - `proxy_on_context_create` と `proxy_on_request_headers` を 1 回のファイバーの出入りで呼ぶ（`arm_store_until` も 1 回）。
   - 要求本文の段は本文が空のとき呼ばない（今も GET で呼んでいないかを確かめる）。
   - 応答の後の段（`proxy_on_log`・`proxy_on_done`・`proxy_on_delete`）を使わないモジュールを設定で宣言できるようにする
     （例: `[[wasm.modules]] phases = ["request_headers", "request_body"]`。既定は今と同じで全部呼ぶ。外部契約なので文書・
     `examples/config.toml` も直す）。SDK のフィルタでも効く。
4. fuel をやめ、打ち切りと yield をエポックで行う案を測る: エポックの専用スレッドを 1 ms 周期にし、
   `epoch_deadline_async_yield_and_update` で協調的に yield、`max_execution_time_ms` は yield の回数で打ち切る。ゲストの実行時間の
   割合（perf）と、エポックの周期を短くした費用（1 ms ごとの原子変数の加算 1 回）を比べる。差が小さければやらない。
   AGENTS.md の「WASM 等の CPU バウンド処理も協調的に yield する」は守る。
5. h2c の退行（上の表）を交互 A/B で確かめ、本当なら原因の変更を特定して直す。

## 進捗

計測は aofish（F-317 の「計測の記録」と同じ条件。サーバは CPU 0-7・ワーカー 8 本、負荷は 8-15、1 回 5 秒の交互）。ノート PC
（4 CPU・共有）より揺れが小さいので、受け入れ条件の A/B もこちらで行った。値は 1 要求あたりのサーバの CPU（µs）。

### 1. 続けて呼ぶコールバックを 1 回のファイバーの出入りで呼ぶ（2026-11-25、b850a901）

- wasmtime は `call_async` ごとにファイバーを作り、エンジンの `Arc`（全ワーカー共有）を複製して捨てる。1 要求の
  `proxy_on_context_create` → `proxy_on_request_headers`、`proxy_on_log` → `proxy_on_delete` の 4 回の出入りが h2c-wasm の CPU の
  約 13% だった。ストアに置いた同期のホスト関数を 1 回 `call_async` し、その中から段を同じファイバーの上で続けて呼ぶ
  （`exports::call_chain`）。1 要求の出入りは 4 回 → 2 回。ホスト関数がゲストの線形メモリを呼ばれるたびに名前で引いていたのも、
  引いたまま持つ（`exports::GuestMemory`）。
- f318d → f319a（交互 2 巡）:

  | 負荷 | epoll | io_uring |
  |---|---|---|
  | h1-wasm（`/baseline11`、512 接続） | 25.60 → 25.05（−2.1%） | 18.84 → 17.46（−7.3%） |
  | pipe-wasm（パイプライン 16） | 8.60 → 7.34（−14.7%） | 7.80 → 7.04（−9.7%） |
  | h2c-wasm | 6.69 → 5.76（−13.9%） | 6.85 → 6.32（−7.7%） |
  | h2-wasm（TLS） | 7.50 → 7.07（−5.7%） | 8.03 → 7.33（−8.7%） |
  | h3-wasm | 8.01 → 7.36（−8.1%） | 8.84 → 8.26（−6.6%） |
  | json-tls（wrk） | 37.89 → 35.26（−6.9%） | 43.18 → 39.34（−8.9%） |
  | h1-plain・h2c-plain（Wasm 無し） | ±1%・−5.0% | ±1%・−4.0% |

### 2. CPU の上限と yield をエポックで測る（`[wasm] metering`。68a04190・bbcaeafb・9a71ad33）

- fuel を外しただけのビルド（`VEIL_EXP_NO_FUEL`、epoll、交互 2 巡）で、Wasm の経路の 1 要求の CPU は h2-wasm −9.4%・h1-wasm −6.9%・
  pipe-wasm −5.2%・h2c-wasm −3.4%・h3-wasm −0.9%（json-tls の −13.8% は wrk の時計の戻り〈B-357〉の回を含む）。fuel はゲストの
  基本ブロックごとに残量を検査する。
- 既定の `metering = "clock"`: 専用スレッドがエポックを進め、走っているゲストは締切でコールバックを呼ぶ。検査の間にスレッドが使った
  CPU 時間（`CLOCK_THREAD_CPUTIME_ID`）を足して `max_execution_time_ms` で打ち切り、実時間の上限で時間切れ、それ以外は譲る。
  - 実時間で数えていた最初の版は、io_uring の E2E で WAF のモジュールの `proxy_on_configure`（正規表現の組み立て。デバッグビルドで
    CPU 約 60 ms）が上限 100 ms を超えて失敗した。CPU を取り合う負荷の下では他のスレッドが走った時間まで数えるので、スレッドの
    CPU 時間にした（区間の最初の検査と Windows だけ実時間を検査の間隔まで）。起動時のコールバックは上限の 10 倍。
  - Pulley（インタープリタ。OpenBSD・NetBSD・FreeBSD aarch64）はゲストが 10〜30 倍遅く、時間で測ると今まで収まっていたモジュールが
    上限を超えるので、既定を fuel にした。
  - **専用スレッドの間隔**: 呼び出しが来ている間ずっと 1 ms ごとに起きると、起きるたびにワーカーが CPU を奪われ、epoll の h1-wasm で
    fuel より 1 要求の CPU が約 19% 多かった（ユーザー時間 +1〜1.5 µs・システム時間 +3〜4 µs、ワーカーの非自発的な切り替えが 5 秒で
    約 7,000 回）。間隔 2 ms で約 +10%、5・10・25 ms では fuel 以下。間隔を 3 段にした（呼び出しが無い間 100 ms、来ている間 10 ms、
    締切をまたいで 5 ms 以上走っているゲストがいる間だけ `yield_interval_us`）。
- 3 段の後の `clock` と `fuel`（9a71ad33、交互 6 巡。json-tls と pipe-wasm・h2c-wasm は 3 巡）:

  | 負荷 | epoll fuel → clock | io_uring fuel → clock |
  |---|---|---|
  | h1-wasm | 30.77 → 30.66（−0.4%） | 19.20 → 18.65（−2.9%） |
  | h2-wasm | 7.38 → 7.37（−0.1%） | 7.52 → 7.44（−1.1%） |
  | h3-wasm | 7.92 → 8.02（+1.3%） | 9.06 → 8.91（−1.7%） |
  | h2c-wasm（3 巡） | 5.90 → 5.90 | 6.06 → 5.82（−4.0%） |
  | pipe-wasm（3 巡） | 8.17 → 8.12 | 7.34 → 7.25（−1.2%） |
  | json-tls（3 巡） | 42.42 → 40.47（−4.6%） | 45.31 → 43.40（−4.2%） |

  fuel を外しただけの上限（−1〜9%）には届かず、差は ±3% の中（json-tls は wrk の揺れが大きい）。既定を `clock` にしたのは、悪化が
  無いことと、`max_execution_time_ms` が名前どおり CPU 時間になること（fuel は命令の数で、ゲストとホストの速さで時間が変わる）。

### 3. v0.10.1 との比較（改修内容の 5）

- v0.10.1 → 9a71ad33（交互 3 巡）: h2c-wasm 20.05 → 6.08（epoll）・21.26 → 5.96（io_uring）、h2-wasm 20.74 → 7.42・22.38 → 7.47、
  h1-wasm 63.53 → 31.79・61.08 → 18.90、h2c-plain 3.78 → 2.47・7.21 → 2.45。上の表の h2c の Wasm の退行は残っていない（F-318・
  F-319 で消えたか、ノート PC の揺れ）。

### 4. Wasm の上乗せと、epoll の上限の負荷での差（2026-11-26）

- 上限の負荷では epoll の h1-wasm が io_uring より 1 要求 10 µs 以上多い（30.7 と 18.7）。システムコールの数（strace、要求あたり）は
  epoll の h1-plain と h1-wasm で同じ（`sendto` 1.01・`read` 1.02・`epoll_wait` 0.03・`poll` 0.03）で、Wasm が足すシステムコールは無い。
- 負荷を一定のレートにして測ると（veil-load `--rate`、ユーザー + システム時間）:

  | 負荷 | epoll | io_uring |
  |---|---|---|
  | h1-plain 上限（約 78 万 rps） | 2.2 + 7.2 = 9.4 | 2.2 + 7.3 = 9.5 |
  | h1-plain 24 万 rps | 5.6 + 19.5 = 25.1 | 6.2 + 20.0 = 26.2 |
  | h1-plain 12 万 rps | 7.8 + 29.4 = 37.2 | 5.7 + 24.0 = 29.7 |
  | h1-wasm 上限（epoll 26 万・io_uring 41 万 rps） | 11.7 + 16.5 = 28.2 | 8.1 + 9.9 = 18.0 |
  | h1-wasm 20 万 rps | 15.5 + 20.8 = 36.3 | 15.3 + 20.0 = 35.3 |
  | h1-wasm 12 万 rps | 20.7 + 28.5 = 49.2 | 10.0 + 20.6 = 30.6 |

  - 同じレートなら両バックエンドの費用はほぼ同じで、1 要求の費用は負荷が上がるほど下がる（1 回の起床で処理する要求が増える）。
    同じレートで比べた Wasm の上乗せは約 9〜11 µs（20 万 rps の h1-wasm と 24 万 rps の h1-plain）で、目安の 15 µs を下回る。
  - io_uring は負荷が上がると送出を 1 回の `io_uring_enter` に束ね、相手（負荷ツール）の起床も束ねられるので、費用が下がり続けて
    41 万 rps まで届く。epoll は応答ごとに `sendto` し、眠っている相手を毎回起こす（ループバックでは送り手が相手の起床の費用を払う。
    F-295）ので、26 万 rps で 8 コアを使い切る。Wasm の費用でレートが下がった所で差が開く。
  - 送出を周回の終わりにまとめる案は reactor（BSD・macOS・Windows と共有）の変更になるので [F-326](F-326-reactor-batched-sends.md) に
    分けた。HttpArena のエントリのイメージ（`full-container`）は epoll なので、エントリを io_uring で作る案も F-320 で測る。

### 見送ったもの

- 改修内容の 1・2（ヘッダの写し・`Arc::from`）: プロファイルで合わせて 2〜3% で、ヘッダの表をゲストのメモリへ直接書く作り直しに
  見合わない。
- 改修内容の 3 の `phases`（応答の後の段を呼ばない設定）: proxy-wasm の SDK は `proxy_on_delete` で context をゲストの表から消すので、
  呼ばないとゲストのメモリが要求ごとに増える。SDK のフィルタには使えない。`proxy_on_log` と `proxy_on_delete` は 1 の連続呼び出しで
  1 回の出入りにした。
- エポックの専用スレッドを `SCHED_IDLE` にする: 負荷の下で走れずに締切が進まなくなる（試していない）。ワーカーがエポックを進める案:
  エンジンのエポックは全ワーカー共有なので、ワーカーが増えると進み方が変わる。実行器と Wasm の層を結ぶ作りも要る（見送り）。

## 受け入れ条件

- ノート PC の交互 A/B で、h1・h2c の Wasm の GET・POST・`/json` の 1 要求あたりの CPU が下がる（目安: 上乗せ 25 µs → 15 µs 以下）。
  allocs/req も記録する。
- Proxy-Wasm の E2E（全段・全プロトコル・epoll）・単体・`cargo test`。fuel をやめる場合は、無限ループのフィルタが
  `max_execution_time_ms` で打ち切られ、他の要求が止まらない試験。

## 関連

- [F-317](F-317-httparena-infra-score-plan.md)（親）・[F-318](F-318-wasm-per-request-shared-writes.md)・B-175・B-173・B-408・F-271
