# F-317: HttpArena の infrastructure で veil の点を上げる（親チケット）

**状態: 未着手（2026-11-22 起票。2026-11-25 に最新の結果〈okena の追加〉で見直し、F-321〜F-325 を追加。調査とチケットまで。改修は人の依頼の後）**

利用者の依頼（2026-11-22）: HttpArena の infrastructure の veil の結果を確認し、性能改善案を作る。ボトルネックは Proxy-Wasm と見ているので
Proxy-Wasm の改善、または veil に大きな変更を加えずに結果を良く見せる方法を考える。

追加の依頼（2026-11-25）: veil-api でも残る h2o との差（Wasm の無い経路の 1 要求あたりの費用）もチケットにする。h2o・nginx を超えるために、
今の veil の機能で HttpArena 用にできることを案に出す。

## 点の付け方

- infrastructure は 9 プロファイル（baseline・pipelined・limited-conn・json-tls・static-tls・baseline-h2・static-h2・baseline-h3・static-h3）。
  プロファイルごとに接続数の rps の平均を infrastructure の中の 1 位で割って 1000 倍し、足す（満点 9000）。h2c・gRPC 等は参考で点に入らない。
- 設定の制限は無い。ハンドラはサーバが読み込むモジュールであればよい（veil は Proxy-Wasm のフィルタ、nginx は C のモジュール）。
- 負荷の掛け方: baseline・limited-conn は gcannon の 4096 接続（GET・POST の Content-Length・chunked の 3 種を回す。limited-conn は
  10 要求で張り直す）、pipelined は 16 本ずつ、json-tls・static-tls は wrk（4096・1024 接続）、HTTP/2 は h2load の `-c 256/1024`
  （baseline は `-m 100`、static は `-m 32`）、HTTP/3 は h2load の `-c 64 -m 64`。サーバと負荷ツールに 64 スレッドずつ。

## 今の結果（HttpArena の `site/data/results`、2026-11-24 の 3388e45。veil は v0.10.1 のイメージ）

| エントリ | baseline | pipelined | limited | json-tls | static-tls | base-h2 | static-h2 | base-h3 | static-h3 | 計 |
|---|---|---|---|---|---|---|---|---|---|---|
| okena | 1000 | 1000 | 1000 | 1000 | 1000 | 1000 | 1000 | 1000 | 1000 | 9000 |
| agensio | 895 | 183 | 932 | 904 | 961 | 878 | 879 | 984 | 732 | 7348 |
| nginx | 826 | 149 | 950 | 692 | 581 | 204 | 470 | 553 | 355 | 4779 |
| fit | 595 | 328 | 569 | 421 | 702 | 396 | 282 | 442 | 787 | 4521 |
| h2o | 916 | 182 | 959 | 648 | 0 | 962 | 207 | 0 | 0 | 3874 |
| varnish | 194 | 84 | 165 | 0 | 252 | 15 | 85 | 0 | 0 | 795 |
| **veil** | 80 | 22 | 114 | 266 | 78 | 33 | 8 | 60 | 14 | **675** |
| caddy | 109 | 28 | 86 | 170 | 128 | 24 | 68 | 41 | 22 | 675 |
| vinyl-cache | 194 | 84 | 152 | 0 | 0 | 0 | 0 | 0 | 0 | 430 |
| （参考）veil-api を infrastructure として数えた場合 | 631 | 126 | 720 | 631 | 755 | 320 | 589 | 301 | 284 | 4358 |

- okena（x86-64 のアセンブリ、io_uring・epoll、TLS・HTTP/2・HTTP/3 も自前）が 2026-11-22 に加わり、全プロファイルで 1 位。前回
  （2026-11-22 の起票時）の veil の 771 点は okena の前の値で、1 位の値が上がったので点は 675 に下がった（veil の rps は同じ）。
- 1 位の rps: baseline 3.98M・pipelined 27.12M・limited 2.77M・json-tls 1.90M・static-tls 1.24M・base-h2 16.49M・static-h2 1.70M・
  base-h3 8.78M・static-h3 1.02M（全部 okena）。veil の rps: 318k・597k・315k・505k・97k・550k・14k・526k・14k。

## 1 要求あたりの CPU（µs、`cpu% × 10000 / rps`。各プロファイルの最初の接続数）

| プロファイル | okena | agensio | h2o | nginx | veil-api | veil |
|---|---|---|---|---|---|---|
| baseline-4096 | 16.4 | 17.2 | 18.1 | 19.6 | 25.8 | 194.7 |
| pipelined-4096 | 2.4 | 12.4 | 13.0 | 15.8 | 19.0 | 104.7 |
| limited-conn-4096 | 20.7 | 23.2 | 23.4 | 23.7 | 32.8 | 193.5 |
| json-tls-4096 | 24.4 | 37.5 | 52.1 | 48.9 | 53.8 | 129.3 |
| static-tls-1024 | 42.6 | 52.6 | — | 91.6 | 68.5 | 114.8（11 コアしか使わない） |
| baseline-h2-256 | 1.3（23 コア） | 1.3（20） | 1.9（30） | 19.0 | 10.2 | 118.7 |
| static-h2-256 | 32.5 | 37.7 | 174.8 | 82.3 | 61.1 | 1,234.8 |
| baseline-h3-64 | 1.6（14） | 1.8（15） | — | 6.3（31） | 13.6（36） | 75.6 |
| static-h3-64 | 26.5 | 45.4 | — | 137.3 | 142.2 | 1,296.3 |

- HTTP/2 の上位 3 つは 20〜30 コアしか使わず、負荷ツール（h2load）の上限（約 16M rps）で止まっている。HTTP/3 の上位 2 つも同じ（約 8.7M）。

## 原因（調査の結果）

1. **静的配信の 3 プロファイル（static-tls・static-h2・static-h3）は Proxy-Wasm と関係ない設定の既定値の不具合**:
   エントリに `[performance]` が無いと open_file_cache の有効期間・最大件数が 0 になり（[B-495](../bugs/B-495-performance-section-default-zero-ofc.md)）、
   キャッシュに一度も当たらない。HTTP/2・HTTP/3 は本体を入れ直すたびに圧縮結果も捨てる（[B-496](../bugs/B-496-content-cache-reinsert-drops-compressed.md)）ので、
   要求ごとに br で圧縮し直す（1 要求 1.2〜1.4 ms の CPU）。設定で回避でき、ノート PC で h2c の静的配信は 710 → 43,216 rps
   （[F-320](F-320-httparena-veil-entry-config.md)）。
2. **Wasm を通る 6 プロファイル（baseline・limited-conn・json-tls・baseline-h2・baseline-h3・pipelined）は Proxy-Wasm が律速**:
   どのプロトコルでも 0.3〜0.6M rps で止まり、1 要求の CPU が veil-api の 4〜8 倍。ノート PC の Wasm の上乗せ（約 25 µs）の 4〜7 倍に
   膨らんでいて、コア数で費用が増える形。要求ごとに wasmtime の型レジストリのロックと全ワーカー共有の `Arc` の参照カウントへ数十回
   書いているのが本命（[F-318](F-318-wasm-per-request-shared-writes.md)）。単発の費用は [F-319](F-319-wasm-per-request-fixed-cost.md)。
3. **pipelined** は `/pipeline`（本文 `ok`）までフィルタで答えている。設定の固定応答で返せる（F-320。okena も同じ）。
4. **HTTP/2 で計測のほぼ全期間（約 5 秒）返らない要求がある**（veil・veil-api とも。h2c でも。結果の `p99_latency` の欄は、
   2026-11-16 の計測では h2load の最大）。少数の要求なので点への影響は小さい見込みだが、利用者の要求が止まる不具合
   （[B-497](../bugs/B-497-h2-p99-stuck-at-run-length.md)）。
5. **Wasm の無い経路（veil-api）にも上位との差がある**（2026-11-25 にチケットにした）:
   - 64 コアでの膨らみ: 本体の経路にも要求ごとに全ワーカー共有の行へ書く箇所がある（ルートのキャッシュの統計と同じ行の世代、ルートの
     `Arc` の複製、静的配信のキャッシュの当たりの数、接続の数）（[F-321](F-321-core-path-per-request-shared-writes.md)）。
   - HTTP/1.1 のパイプライン: 16 本まとめて届いても応答を 1 本ずつ送る（[F-322](F-322-h1-pipelined-response-coalescing.md)。okena は
     まとめて送って 2.4 µs/要求・27.1M rps）。
   - HTTP/2: ストリームごとにタスク・チャネル・通知・ヘッダの表を作る固定費（[F-323](F-323-h2-per-stream-fixed-cost.md)）。
   - HTTP/3: 1 要求 13.6 µs（nginx 6.3、agensio 1.8）。64 接続をハッシュでワーカーへ配るので、約 23 ワーカーに接続が来ない
     （[F-324](F-324-h3-per-request-cost.md)）。
6. **Proxy-Wasm のままでは、上の全部を直しても h2o に届かない見込み**（下の試算の S7）。上位のインフラのエントリは全部ハンドラを
   サーバに組み込んでいる（okena は `make MODULES=httparena.inc`、agensio は CMake の `-DAGENSIO_HTTPARENA=ON`、nginx は C のモジュール、
   h2o は libh2o に C のハンドラを登録）。veil も今の機能（ハンドラを組み込み、`veil.toml` の `type = "Handler"` で指す）で同じ形に
   できる（[F-325](F-325-httparena-veil-compiled-handler-module.md)）。

## 試算（infrastructure の総合点。他のエントリは今の値のまま。`docs/artifacts/benchmark/httparena-perf/scripts/scen.py`）

| 段階 | 前提 | 点 | 順位（9 エントリ） |
|---|---|---|---|
| S0 | 今 | 675 | 7 |
| S1 | F-320 の 1〜3（静的が veil-api 並み・`/pipeline` を固定応答。Wasm のまま） | 約 2,310 | 6 |
| S2 | S1 + F-318（Wasm の上乗せがノート PC 並みの 25 µs: baseline 1.2M・limited 1.07M・json-tls 0.74M・base-h2 1.7M・base-h3 1.0M） | 約 3,050 | 6 |
| S7 | S2 + F-322（pipelined 12M）+ F-321・F-323・F-324 の一部（base-h2 3M・base-h3 1.6M） | 約 3,510 | 6 |
| S3 | F-325（ハンドラを組み込む）+ F-320 = veil-api の値 | 約 4,360 | 5（h2o 3,874 を超える） |
| S4 | S3 + F-322（pipelined 12M） | 約 4,670 | 4（fit 4,521 を超える） |
| S5 | S4 + F-323（base-h2 10M） | 約 4,960 | 3（nginx 4,779 を超える） |
| S6 | S5 + F-324（base-h3 5M） | 約 5,230 | 3 |

- 12M・10M・5M 等は目標の置き方で、実測ではない。上位 2 つ（okena 9,000・agensio 7,348）は 1 要求 1〜2 µs で別格。
- veil-api の静的配信は File バックエンドではなくライブラリの `serve_dir` なので、静的の値は目安。

## 進め方（提案）

1. 計測の土台: aofish の伸び方の試験（ワーカー 1〜16 本）、HttpArena と同じ負荷ツール（gcannon・wrk・h2load・h2load-h3 の Docker
   イメージ）、perf。改修前の値（main）を記録する。
2. **B-495・B-496・B-498**（小さい修正）: 設定を書かない利用者にも効く（B-498 は文書どおりの暗号スイートの選び方）。
3. 本体の経路: **F-321 → F-322 → B-497 → F-323 → F-324**（veil-api と、F-325 の版の veil に効く）。
4. Proxy-Wasm: **F-318 → F-319**（Proxy-Wasm の利用者に効く。エントリを Proxy-Wasm のまま出す場合の点にも効く）。F-319 の計測で、
   epoll（コンテナのイメージ）の上限の負荷の差は送出の束ね方だと分かったので **F-326** に分けた（2026-11-26）。
5. エントリ（手元だけ。提出しない）: F-320（Proxy-Wasm の版の設定と今の機能の調整）と F-325（ハンドラを組み込む版）を作り、HttpArena の
   検証と計測で比べる。veil-api のエントリも新しいコードで測る。
6. main へマージ・push（タグは人の判断）。HttpArena への再提出（どちらの版を出すかを含む）は人の確認の後（F-269）。

## 計測の環境と記録

- HttpArena の結果: 公開リポジトリの `site/data/results/*.json`。2026-11-22 の写しは `docs/artifacts/benchmark/httparena-perf/httparena-results/`、
  2026-11-24 の写し（okena を含む）は `httparena-results-20261124/`（取得時のコミットは各 `REPO_COMMIT.txt`）。
  計測は 64 スレッド（cpuset 0-31,64-95）・5 秒・`--network host --security-opt seccomp=unconfined`。負荷は gcannon（h1）・h2load（h2・h3）・wrk（TLS）。
- 手元: ノート PC（4 CPU、共有の機械で ±5% 程度揺れる）、veil-load（h1・h2c）、サーバの CPU は `/proc/<pid>/task/*/schedstat`。
  スクリプト（`infra_score.py`・`scen.py`・`cpu_table.py`・`measure.py`）・設定・結果は `docs/artifacts/benchmark/httparena-perf/`（git 管理外）。

## 計測の記録

### 土台（aofish、2026-11-25）

- aofish（16 スレッド = 8 コア × SMT、WSL2）。サーバは CPU 0-7（物理 4 コア）、負荷ツールは 8-15。計測は WSL のロック
  `/tmp/vp-bench.lock` を取って行う。ビルドは GitHub の枝を取り出して `--features full`（io_uring）と `full,epoll` の 2 つ。
- 設定は HttpArena のエントリと同じ（静的配信は `sendfile`・`open_file_cache`・`static_file_cache`・圧縮、`/baseline11`・`/pipeline`・
  `/json` は Proxy-Wasm のフィルタ、`/plain3` は Wasm を通らない `Respond`）。ワーカーは既定（8 本）。
- 負荷: veil-load（h1・パイプライン 16 本・h2c）、h2load（h2・静的 h2）、wrk（TLS の json・静的）、h3load（h3）。1 回 5 秒・1 秒の
  暖機、2 つのビルドを交互に 2 巡。サーバの CPU は `/proc/<pid>/task/*/schedstat`（1 要求の µs）。
- 道具と結果は `docs/artifacts/benchmark/httparena-perf/scripts/aofish/`（`bench.py`・`abcmp.py`・`scale.sh`・`prof*.sh`）と
  `results/aofish/`（git 管理外）。
- wrk の TLS（json-tls・static-tls）で、どのビルドでも時々「timeout が接続の数（234〜256）・計測が 3 秒で終わる」回が出る。WSL2 の
  時計が戻る既知の現象（B-357）で、その回の rps は水増しされる。比較から外す。

### B-495・B-496・B-498（main → b498、2 巡の平均。rps / 1 要求の CPU µs）

| 負荷 | epoll main | epoll b498 | io_uring main | io_uring b498 |
|---|---|---|---|---|
| static-h2（h2load 64 × 32） | 6,917 / 1,107 | 362,752 / 20.8 | 6,454 / 1,178 | 339,084 / 22.3 |
| static-h3（h3load 16 × 64） | 1,396 / 674 | 31,452 / 30.1 | 1,390 / 681 | 30,178 / 31.5 |
| static-tls（wrk 256） | 125,090 / 58.7 | 223,640 / 30.9 | 142,867 / 66.7 | 226,144 / 31.6 |

その他の負荷は ±5% の揺れの中。

### B-499・B-500（失った要求）

- io_uring のビルドで、Wasm のルートへの HTTP/1.1 で接続の集約（F-303）が接続を動かすたびに要求を失っていた。B-499（Wasm のローカル
  レスポンスに `Connection: close` が無い）を直した後も、パイプライン（16 本）で 5 秒に約 930 要求（張り直し 62 回 × 15 本）を失い、
  B-500（パイプラインの接続を動かさない）で 0 になった（h1-wasm: 250 → 0、pipe-wasm: 1,936〜4,109 → 0）。

### F-321（b498 → f321）

| 負荷 | epoll rps | epoll CPU | io_uring rps | io_uring CPU |
|---|---|---|---|---|
| h1-plain | +11.2% | −10.8% | +8.4% | −6.0% |
| pipe-plain | +12.2% | −11.8% | +26.3% | −21.9% |
| h2c-plain（64 × 100） | +17.8% | −14.7% | +20.8% | −17.7% |
| h2-plain（h2load 64 × 100） | +11.8% | −12.1% | +6.4% | −12.1% |

Wasm を通る負荷・静的配信・HTTP/3 は ±4% の中（Wasm の要求ごとの費用は F-318・F-319）。

伸び方（ワーカー 1・2・4・8 本、使わないサーバの CPU は空回りで埋めてクロックをそろえる。1 要求の CPU µs、io_uring b498 → f321）:

| 負荷 | 1 本 | 2 本 | 4 本 | 8 本 |
|---|---|---|---|---|
| h2c-plain | 3.03 → 3.05 | 3.28 → 3.26 | 3.17 → 2.70 | 3.37 → 2.87 |
| h1-plain | 36.0 → 42.5 | 27.0 → 34.3 | 19.8 → 16.7 | 11.5 → 10.3 |

- h2c はワーカーの数によらず平ら（8 コアの aofish では共有の行の奪い合いはまだ小さい。f321 は 4・8 本で 15% 下がる）。
- h1 はワーカーが少ないほど 1 要求が重い（逆向き）。ワーカーが少ないと負荷ツールのスレッドが眠りがちで、ループバックの送信が相手の
  起床（別の CPU への IPI）まで払うため（F-295）。1 本と 8 本の比較は h1 では意味を持たない。io_uring のパイプラインは epoll より
  ワーカーが少ないときに大きく重い（1 本で 42 µs と 12 µs）。原因は未調査（F-322 の後に見る）。

### F-322（f321 → f322。B-500 を含む）

| 負荷 | epoll rps | epoll CPU | io_uring rps | io_uring CPU |
|---|---|---|---|---|
| pipe-plain（`Respond`、256 × 16） | +22.7% | −44.0% | +35.5% | −45.7% |
| pipe-wasm（Wasm、256 × 16） | +87.5% | −47.3% | +60.7% | −38.0% |

その他は ±3% の中（h1-wasm の io_uring の −5.6% は次の A/B で見直す）。

## 子チケット

- [B-495](../bugs/B-495-performance-section-default-zero-ofc.md)・[B-496](../bugs/B-496-content-cache-reinsert-drops-compressed.md)・
  [B-497](../bugs/B-497-h2-p99-stuck-at-run-length.md)・[B-498](../bugs/B-498-tls-cipher-suites-order-not-server-preference.md)・[F-318](F-318-wasm-per-request-shared-writes.md)・
  [F-319](F-319-wasm-per-request-fixed-cost.md)・[F-320](F-320-httparena-veil-entry-config.md)・
  [F-321](F-321-core-path-per-request-shared-writes.md)・[F-322](F-322-h1-pipelined-response-coalescing.md)・
  [F-323](F-323-h2-per-stream-fixed-cost.md)・[F-324](F-324-h3-per-request-cost.md)・
  [F-325](F-325-httparena-veil-compiled-handler-module.md)・[F-326](F-326-reactor-batched-sends.md)

## 関連

- F-269（HttpArena への提出）・F-294（事前圧縮）・B-175（Wasm のインスタンスの再利用）・F-188（API ライブラリ）
