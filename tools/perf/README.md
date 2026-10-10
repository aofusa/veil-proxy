# perf — Veil vs nginx パフォーマンス計測ハーネス

`veil:glibc` / `veil:musl`（full features ビルド）と `nginx:alpine` を **同一ネットワーク上のコンテナ間通信** で計測し、HTTP/1.1（`wrk`）と HTTP/2（`h2load`）のスループット・レイテンシ・CPU/メモリ使用量を TSV に集約するハーネスです。

すべて **docker コマンドのみ** で完結し、ホストへの追加インストールは不要です（証明書生成に `openssl` を使う場合を除く）。

計測結果は [docs/perf/](../../docs/perf/) を参照してください（Linux/Docker の生データは [`docs/perf/results_raw.tsv`](../../docs/perf/results_raw.tsv)、**FreeBSD ネイティブ計測の生データは [`docs/perf/freebsd_results_raw.tsv`](../../docs/perf/freebsd_results_raw.tsv)**）。バックログ上は [F-58](../../docs/backlog/features/F-58-perf-report-glibc-musl-nginx.md) の再現ハーネスです。

---

## 構成

| パス | 役割 |
|------|------|
| `run_perf.sh` | 計測オーケストレータ（nginx → veil glibc/musl × 全バリアント × 反復）。完了後に集計も実行 |
| `gen_configs.sh` | 計測用 `config.toml` バリアントを生成（**完全直交 2⁴=16** + full features 機能ショーケース `feat_*` + **全プロトコル×全機能マトリクス**（F-114: `h2_1_proxy_*` / `h3_file_*` / `h3_proxy*` / `grpc_h2_*` / `grpc_h3*`）+ **h2c（平文 HTTP/2 prior knowledge）**（`h2c_file` / `h2c_proxy`）） |
| `h2c_proxy_lab.sh` | h2c 単一構成のラボハーネス（`run_perf.sh` は 1 構成の計測が終わるとコンテナを落とすため、**負荷をかけたまま**の CPU 内訳・syscall 集計・メモリ推移が取れない）。`up`/`upnginx` で veil と比較対象 nginx を起動しっぱなしにし、`load`（rps）・`cpu`（クライアント/プロキシ/上流の 3 者同時サンプル）・`strace`（**1 リクエストあたりの syscall 回数**）・`mem`（持続負荷中の RSS 推移）を差し込む。ペイロードは `/`（54,576B）と `/3b.html`（3B）の 2 サイズを配信し、**バイト単価と固定費を切り分けられる**ようにしてある |
| `analyze_results.sh` | 反復生データ（`results_raw.tsv`）を **median±stdev** に集計し Markdown を出力。**Linux ハーネスの 11 列形式専用**で、FreeBSD ネイティブ計測（8/10 列）を渡すと列位置がずれるため**明示エラーで停止する**（黙って全行 0.0 を出さない）。`#` 始まりの節見出し・列凡例・ヘッダ行は集計対象外 |
| `configs/*.toml` | 生成済みバリアント（`gen_configs.sh` で再生成可能）。**`*_compression_cached` は F-169 で追加した「静的キャッシュ有効」版**で、`static_file_cache` + `open_file_cache` を有効にしないと F-169 の圧縮結果キャッシュは有効化条件を満たさず一切効かない（既存の `*_compression` はキャッシュ無効時のコストを測るため据え置き） |
| `nginx/nginx.conf` | 比較対象 nginx の設定（`access_log off` で公平化。平文 8080 で `listen 8080; http2 on;` により h2c も有効化し、veil の h2c 専用リスナーと条件を揃える） |
| `results/` | 計測結果（`results_raw.tsv` / `results_summary.md` / `logs/` は `.gitignore` 対象）。公開する生データは [docs/perf/results_raw.tsv](../../docs/perf/results_raw.tsv) へコピーしてコミットする（サマリは [docs/perf/README.md](../../docs/perf/README.md)） |

計測に必要な静的アセットは **`docker/assets/`** を参照します（このディレクトリには複製しません）。

- TLS 証明書: `docker/assets/ssl/{cert.pem,key.pem}`
- 配信コンテンツ: `docker/assets/www/index.html`
- seccomp 許可リスト: `docker/assets/security/seccomp.json`（io_uring 許可のため必須）

---

## 前提条件

| 項目 | 内容 |
|------|------|
| Docker | daemon が起動していること |
| Veil イメージ | `veil:glibc` / `veil:musl`（[docker/README.md](../../docker/README.md) の手順で `--build-arg CARGO_FEATURES='full'` ビルド） |
| nginx イメージ | `nginx:alpine`（初回 `docker run` で自動 pull） |
| wrk イメージ | `williamyeh/wrk:latest`（HTTP/1.1 負荷、自動 pull） |
| h2load イメージ | `local/h2load:latest`（HTTP/2 負荷。`nghttp2` の `h2load` を含むイメージを事前に `local/h2load:latest` としてビルド） |
| h2load-h3 イメージ | `local/h2load-h3:latest`（**HTTP/3 負荷**。QUIC 対応 h2load。`docker build -t local/h2load-h3:latest tools/perf/h2load-http3` でビルド。未ビルドなら http3 計測は自動スキップ） |
| k6 イメージ | `grafana/k6:latest`（**gRPC / WebSocket 負荷**、自動 pull） |
| grpcbin イメージ | `moul/grpcbin:latest`（gRPC 上流エコー、自動 pull） |
| echo-server イメージ | `jmalloc/echo-server:latest`（WebSocket エコー上流、自動 pull） |
| TLS 証明書 | `docker/assets/ssl/` に自己署名証明書（[docker/README.md](../../docker/README.md) の openssl 手順で生成） |

> `local/h2load:latest` は環境に h2load を含むイメージがない場合、`nghttp2` を含む任意の Dockerfile から `docker build -t local/h2load:latest .` で用意してください（`--entrypoint h2load` で起動します）。
>
> **HTTP/3 計測**には QUIC 対応 h2load が必要です。既定の `local/h2load` は ngtcp2 非搭載で
> HTTP/3 を計測できないため、`docker build -t local/h2load-h3:latest tools/perf/h2load-http3` で
> ngtcp2/nghttp3/quictls 組み込みの h2load をビルドしてください。未ビルドの場合、http3 構成は
> スキップされます（gRPC/WebSocket/その他は影響しません）。
>
> **Docker が無い環境（FreeBSD 等）で HTTP/3 を計測する場合**は、上記 QUIC 対応 h2load の
> 代わりに `quinn` + `h3`（テスト・計測ツール向けの HTTP/3 ライブラリ。本番データプレーンの
> `quiche` とは別方針、AGENTS.md 参照）で実装した自前クライアント
> `tools/perf/h3load`（`h2load` 互換の CLI・出力書式、`cargo build --release --manifest-path
> tools/perf/h3load/Cargo.toml` でビルド）を使えます。`tools/perf/bsd/run_perf_bsd.sh`（F-176 で FreeBSD / NetBSD / OpenBSD 共通化。旧パス `tools/perf/freebsd/` は互換用の転送スクリプト）は
> これを優先して使い（無ければ `h2load --h3` にフォールバック）、詳細は同スクリプトのヘッダ
> コメントを参照してください。
>
> h3load はクライアント UDP ソケットの送受信バッファを通る最大値（2MB から 1/8 ずつ下げる）まで
> 広げる（B-95）。BSD の既定（FreeBSD 42KB）のままだとサーバの応答バーストをクライアント側で
> 取りこぼし、それがサーバの損失回復（PTO の指数バックオフ）として計測に乗って
> **1 接続が 30 秒止まる**（10 秒の計測が 35〜39 秒かかり、errored が 32 の倍数で出る）。
> 切り分け用に `H3LOAD_SLOW_MS=<ms>` を設定すると、それを超えたリクエストの経過時間・
> エラー・quinn の接続統計（RTT・損失・送受信データグラム数）を標準エラーへ出す。

---

## 実行方法

```sh
# 1) 計測用 config バリアントを生成（configs/*.toml を再生成）
bash tools/perf/gen_configs.sh

# 2) 計測を実行（nginx → veil:glibc → veil:musl × 各バリアント）
bash tools/perf/run_perf.sh
```

### `full-container`（epoll reactor）ビルドの計測

`BUILDS` は `veil:<name>` というイメージ名と `veil_<name>` というラベルにそのまま使われる。
したがって **`full-container` feature セット（`full` + `epoll` = readiness reactor）のイメージを
`veil:container` としてビルドしておけば、`BUILDS` に足すだけで同じマトリクスを計測できる**。

```sh
# full-container ビルドのイメージを作る（io_uring 既定ビルドと同一コミットから）
docker build -f docker/Dockerfile.glibc -t veil:container \
    --build-arg CARGO_FEATURES='full-container' .

# io_uring 既定ビルドと reactor ビルドを同一スイートで比較する
BUILDS='glibc container' ITERATIONS=3 bash tools/perf/run_perf.sh
```

**必ず同一コミットから両方をビルドすること**（片方だけ古いイメージを使うと、比較しているのが
ランタイムバックエンドの差なのかコード差なのか分からなくなる）。reactor で動いていることは
起動ログの `enable_io_uring_restrictions is set but this build uses the reactor (epoll) runtime
backend` 警告で確認できる（2026-10 の最新フルスイートは glibc / musl の 2 ビルドで、
`full-container` は含めていない）。

リポジトリのどこから実行しても、スクリプトが自身の位置からリポジトリルートと `docker/assets/` を解決します。

### 手元の `docker run` で挙動を再現するときの注意（2026-08-27）

`run_perf.sh` の `start_veil` は計測用設定を **`/etc/veil/conf.d/config.toml`** へマウントする。
手動で再現するときも**必ず同じマウント先を使うこと**。`/etc/veil/config.toml` へマウントすると
イメージ同梱の既定設定（静的 File ルート）が生き残り、**意図した構成とは別の経路を測ってしまう**。

さらに **snap 版 docker はリポジトリ外のパス（`/tmp/claude-*` 等）を bind mount できず、
エラーを返さずに空ディレクトリを作る**。設定ファイルは**リポジトリ配下**に置くこと
（`run_perf.sh` が `$LOGDIR` に runtime 設定を書くのはこのため）。

この 2 つが重なると「設定を変えたのに何も変わらない」という形で現れる。
**差し替え検証では、まず起動ログで意図した経路（`Proxy` か `SendFile` か）を確認する。**


各構成は**ウォームアップ後に `ITERATIONS`（既定 3）回**計測します。生データは `results/results_raw.tsv`
（1 反復 1 行）に、median±stdev 集計は `results/results_summary.md` に保存され、後者が標準出力にも表示されます。
個別の負荷ツールログと CPU/メモリサンプルは `results/logs/` に残ります。

集計だけをやり直す場合:

```sh
bash tools/perf/analyze_results.sh tools/perf/results/results_raw.tsv
```

### 負荷パラメータ（環境変数で上書き可）

| 変数 | 既定 | 意味 |
|------|------|------|
| `ITERATIONS` | `3` | 各 (config, proto) の反復回数（median±stdev 集計用） |
| `WRK_ARGS` | `-t4 -c100 -d10s --timeout 5s --latency` | HTTP/1.1（wrk）: 4 スレッド・100 接続・10 秒 |
| `H2_ARGS` | `-n 30000 -c100 -m10` | HTTP/2（h2load）: 30000 リクエスト・100 接続・多重化 10 |
| `H3_ARGS` | `--alpn-list=h3 -n 30000 -c100 -m10` | HTTP/3（h2load QUIC）: ALPN=h3・30000 リクエスト・100 接続・多重化 10 |
| `H2C_PORT` | `8080` | h2c（平文 HTTP/2 prior knowledge）: veil の `h2c_listen` / nginx の `listen 8080 http2` のポート |
| `H2C_ARGS` | `$H2_ARGS`（既定 `-n 30000 -c100 -m10`） | h2c（h2load 平文 prior knowledge）: 既定は HTTP/2 と同条件 |
| `K6_VUS` | `50` | gRPC / WebSocket（k6）並列仮想ユーザ数 |
| `K6_DURATION` | `10s` | gRPC / WebSocket（k6）計測時間 |
| `CONFIG_GLOB` | `*` | 計測対象 config を絞り込む glob（例: `h3_*` / `grpc_*` / `h2_1_proxy_*`）。既定は全構成 |
| `BUILDS` | `glibc musl` | 計測対象の veil ビルド。改修の A/B 中は片方のイメージしか作り直さないため、`BUILDS=glibc` で古い `veil:musl` を混ぜないようにする |

> 全構成（65+）× glibc/musl × 反復 のフルスイートは時間がかかります。素早く確認したい場合は
> `ITERATIONS=1` や `CONFIG_GLOB='h3_*'`（対象構成のみ）で実行してください。

---

## 計測バリアント（`gen_configs.sh`）

いずれも同一の静的ファイル（`/var/www/index.html`）を `File` アクションで配信し、4 因子
**`http2 × ktls × reuseport_balancing(cbpf/kernel) × open_file_cache`** を **完全直交（2⁴=16 構成）**
で組み合わせます。アクセスログは `logging.level = "warn"` で抑止し、nginx の `access_log off` と条件を揃えます。

バリアント名は `h2_<0|1>_ktls_<0|1>_lb_<cbpf|kernel>_ofc_<0|1>`。`run_perf.sh` は名前の `h2_1` から
HTTP/2 負荷（h2load）の要否を判定します（`h2_0_*` は wrk のみ）。

### full features 機能ショーケース（`h2_1_feat_*` / `h2_0_feat_l4`）

直交表に加え、full features に含まれる各機能のオーバーヘッドを計測する `feat_*` 構成を生成します。
いずれも共通ベース（**HTTP/2 有効・kTLS 無効・kernel LB**。kTLS はコンテナ環境と相性が悪いため
feat 系では既定オフ）へ **1 機能だけを重ね**、ベースライン比の相対オーバーヘッドを見ます（F-89）。

| 構成 | 機能 | 計測対象 |
|------|------|----------|
| `h2_1_feat_compression` | compression | zstd/br/gzip 圧縮（Accept-Encoding 付与） |
| `h2_1_feat_cache` | cache | インメモリキャッシュ |
| `h2_1_feat_proxy` | 逆プロキシ | perf-backend(nginx) へ中継 |
| `h2_1_feat_buffering` | buffering | 逆プロキシ + full バッファリング |
| `h2_1_feat_wasm` | wasm | パススルー Proxy-Wasm フィルタ 1 枚の wasmtime オーバーヘッド |
| `h2_1_feat_metrics` | metrics | Prometheus カウンタ/ヒストグラム更新コスト |
| `h2_1_feat_access_log` | access-log | JSON 構造化ログのフォーマット + 非同期出力 |
| `h2_1_feat_rate_limit` | rate-limit | スライディングウィンドウ判定コスト |
| `h2_1_feat_admin` | admin | Admin API 有効化時のルーティング判定 |
| `h2_1_feat_otel` | opentelemetry(+metrics) | OTLP エクスポートスレッドのデータプレーン干渉 |
| `h2_0_feat_l4` | l4-proxy | L4 TCP 素通し（**平文 9080** を wrk で計測。`run_perf.sh` が URL を切替。readiness 確認も 443 に加えて 9080 の 200 応答を待つ） |
| `h2_1_feat_http3` | http3 | HTTP/3 (QUIC) 静的配信。**h2load QUIC**（`--alpn-list=h3`）で UDP/QUIC 計測 |
| `h2_1_feat_grpc` | grpc-full | gRPC unary（**k6**）→ veil(TLS h2) → grpcbin(h2c) 中継のフレーミング/中継コスト |
| `h2_1_feat_websocket` | websocket | WebSocket エコー（**k6**）の Upgrade + フレーム転送コスト |

wasm 構成は `docker/assets/wasm/passthrough_filter.wasm`（`examples/wasm-filters/passthrough-filter/`）を
`run_perf.sh` が `/etc/veil/wasm:ro` にマウントして使用します。

http3 / grpc / websocket は専用クライアントで計測します（`run_perf.sh` が構成名から自動判定）:

- **http3**: QUIC 対応 h2load（`local/h2load-h3`）で `--alpn-list=h3` の QUIC 負荷。
  `http2_enabled=true` も併設するため同一構成で HTTP/1.1・HTTP/2・HTTP/3 を比較できます。
- **grpc**: `grafana/k6` の gRPC クライアント（[k6/grpc.js](k6/grpc.js) + [k6/hello.proto](k6/hello.proto)）が
  `hello.HelloService/SayHello` を veil 経由で呼び、上流 `moul/grpcbin`（h2c）へ中継。
  gRPC は Content-Type 検出でフルパス保持 + h2c 中継（B-40）。k6 は完了ストリームごとに
  RST_STREAM を送るため、構成で `[http2] max_rst_stream_per_second` を大きく設定し
  Rapid Reset 対策（CVE-2023-44487）の誤検知を避けています。
- **websocket**: `grafana/k6` の WebSocket クライアント（[k6/websocket.js](k6/websocket.js)）が
  `/.ws` でフレームを往復し、上流 `jmalloc/echo-server` がエコー。

計測結果とボトルネック分析は [docs/perf/README.md](../../docs/perf/README.md)
を参照（要約: **TLS 終端が支配的コスト**で L4 平文は最大 2.2 倍、L7 機能ロジックのオーバーヘッドは
ノイズ範囲内・全構成 Non-2xx=0、逆プロキシのみバックエンドホップで −15%）。

### 全プロトコル×全機能 網羅マトリクス（F-114）

`docs/artifacts/perf_coverage_report.md` の網羅性評価を受け、上記 File+機能 に加えて
**Proxy / HTTP/3 / gRPC(H2/H3)** へ各機能を重ねた組み合わせを生成します（`gen_configs.sh` の
ヘルパー関数でループ生成）。命名からプロトコル・アクション・機能が一意に定まります。

| 命名パターン | プロトコル | アクション | 機能 |
|--------------|-----------|-----------|------|
| `h2_1_proxy_<feat>` | HTTP/1.1 & HTTP/2 | Proxy(perf-backend) | cache / compression / wasm / metrics / access_log / rate_limit / otel |
| `h3_file_<feat>` | HTTP/3(+H1/H2) | File | cache / compression / wasm / metrics / access_log / rate_limit / admin / otel |
| `h3_proxy[_<feat>]` | HTTP/3(+H1/H2) | Proxy(perf-backend) | （ベース）+ buffering / cache / compression / wasm / metrics / access_log / rate_limit / otel |
| `grpc_h2_<feat>` | gRPC over H2 | Proxy(perf-grpc, h2c) | wasm / metrics / access_log / rate_limit / otel |
| `grpc_h3[_<feat>]` | gRPC over H3 | Proxy(perf-grpc, h2c) | （ベース）+ wasm / metrics / access_log / rate_limit / otel |

- `h3_*` 構成は `http2_enabled=true` も併設するため、**同一構成で HTTP/1.1・HTTP/2・HTTP/3** を計測します。
- **gRPC over HTTP/3（`grpc_h3*`）** は k6 が gRPC over QUIC/H3 をネイティブ非対応のため、
  `run_perf.sh` が **フェイルセーフでスキップ**し `NA` を出力します（計測全体は停止しません）。
- **L4 + metrics/access_log/rate_limit**（レポートのグループD）は `L4ListenerConfig` に
  per-listener の該当設定が無く（L7 の `[route.*]` / グローバル `[prometheus]` の責務）、
  設定として表現できないため **N/A**（L4 ベース `h2_0_feat_l4` 1 種のみ維持）。

### h2c（平文 HTTP/2 prior knowledge、`h2c_file` / `h2c_proxy`）

veil の平文リスナー `h2c_listen` は **h2c 専用**で HTTP/1.1 を受け付けません（`[server].http` は
HTTPS への 301 リダイレクト専用のため計測に使えません）。そのため h2c は専用の 2 構成を用意し、
`gen_srv_head` の第 2 引数（h2c ポート）で `h2c_enabled = true` / `h2c_listen = "0.0.0.0:8080"` を
`[server]` に追加します。

| 構成 | プロトコル | アクション | 計測対象 |
|------|-----------|-----------|----------|
| `h2c_file` | h2c | File | 平文 HTTP/2 prior knowledge での静的配信 |
| `h2c_proxy` | h2c | Proxy(perf-backend) | 平文 HTTP/2 prior knowledge での逆プロキシ中継 |

比較対象の nginx も `nginx/nginx.conf` の平文 8080 サーバブロックで `http2 on;` により h2c を
有効化しており、`/proxy/` へのアクセスで `h2c_proxy` 相当の逆プロキシ経路も計測します
（`run_perf.sh` が nginx ベースラインでも `h2c_file` / `h2c_proxy` と同じ config 名で結果を出力し、
比較できるようにしています）。クライアントは通常の HTTP/2 と同じ `h2load` を `http://` スキームで
使い、prior knowledge（ALPN ネゴシエーションなし）で接続します。

> 網羅マトリクスを加えた全構成（65+）× glibc/musl × 反復 のフルスイートは非常に時間がかかります。
> `CONFIG_GLOB` 環境変数で対象を絞り込めます（例: `CONFIG_GLOB='h3_*' bash tools/perf/run_perf.sh`
> で HTTP/3 構成のみ、`CONFIG_GLOB='grpc_*'` で gRPC 構成のみ）。既定は全構成。

主な着目点（[docs/perf/README.md](../../docs/perf/README.md) 参照。**最新のフルスイートは
2026-10-07（`3746594`、全 69 構成 × glibc/musl × 3 反復 = 810 計測、Non-2xx = 0、NA なし）**）:

- **最良構成 `h2_1_ktls_0_lb_kernel_ofc_1`**（HTTP/2 有効・kTLS 無効・kernel LB・OFC 有効）で
  HTTP/1.1 glibc 9,667 / musl 9,527 vs nginx 6,437 = **1.50×**、HTTP/2 glibc 7,243 / musl 7,166
  vs nginx 5,932 = **1.22×**。
- **h2c は静的 2.39×・逆プロキシ 1.75×**（`h2c_file` 24,135 / `h2c_proxy` 10,858 vs
  nginx 10,080 / 6,188）。L4 平文素通しは **2.04×**。圧縮結果キャッシュ有効の
  `h2_1_feat_compression_cached` は HTTP/2 で **3.02×**。
- FreeBSD 14.3 aarch64 のネイティブ計測（`tools/perf/freebsd/`）は 16 項目中 12 項目で nginx 以上。
- **`feat_proxy` / `feat_buffering` の「対 nginx」を額面どおり読まないこと。**
  nginx ベースライン（`base` 構成）は **TLS 静的配信**であり逆プロキシではない
  （`nginx/nginx.conf` の 443 サーバは `root /var/www`）。したがってこれらの比は
  「veil の逆プロキシ」対「nginx の静的配信」であって同条件比較ではない。
  **プロキシ同士の同条件比較になっているのは h2c だけ**（nginx 側も `/proxy/` で中継する）。
- 機能別オーバーヘッド（HTTP/2・基準 `h2_1_ktls_0_lb_kernel_ofc_0` = 6,946、2026-10-07）は
  観測系（metrics / otel / admin / rate-limit / access-log / wasm / cache）が **96〜100%**、
  proxy / buffering が **86〜87%**（バックエンドホップそのもの）、
  キャッシュ無効の compression が **56%**（54,576B を毎リクエスト zstd 圧縮する CPU バウンド処理）、
  圧縮結果キャッシュ有効（`compression_cached`）は **258%**（圧縮後 16KB を返すため）。
- glibc と musl の差は代表構成のいずれでも **数 % 以内でノイズ範囲**。
- コンテナ（veth）では **kTLS 有効が不利**（`ktls_1` は rustls 比で低下）。
- `cbpf` と `kernel` の振り分けは同等（`h2_1_ktls_0_lb_{cbpf,kernel}_ofc_1` の HTTP/1.1 で 9,721 / 9,667）。B-76 以前は cBPF が全接続をワーカー 0 に寄せていた。
- 過去計測（2026-07-06）で異常だった「`feat_proxy` HTTP/1.1 の wrk 完了 0」「`kernel` +
  HTTP/2 + `ktls_1` の激減」「HTTP/2 逆プロキシの 5xx 混入」は、それぞれ
  B-25（splice `SPLICE_F_MORE`）/ B-27（`write_all` short write）/ B-28（バックエンド接続
  プーリング欠如）として **v0.5.0 で修正済み**（[docs/backlog/backlog.md](../../docs/backlog/backlog.md) 参照）。

---

## 出力フォーマット

`results_raw.tsv` の行順は **(1) nginx ベースライン → (2) veil_glibc の各構成 →
(3) veil_musl の各構成** の順であることが保証される（`run_perf.sh` の実行順そのもの）。

### 生データ `results/results_raw.tsv`（1 反復 1 行）

```
target  config  proto  iteration  req_per_sec  transfer  lat_avg  lat_p99  non2xx  cpu_pct  mem_mb
```

- `target`: `nginx` / `veil_glibc` / `veil_musl`
- `config`: バリアント名（nginx は `base` 固定）
- `proto`: `http1.1`（wrk）/ `http2`（h2load）/ `http3`（h2load QUIC）/ `h2c`（h2load 平文 prior knowledge）/ `grpc`（k6）/ `websocket`（k6）
- `iteration`: 反復番号（1..`ITERATIONS`）
- `cpu_pct` / `mem_mb`: 各反復の負荷中に `docker stats` を 3 回サンプルした平均

### 集計 `results/results_summary.md`（`analyze_results.sh`）

`(target, config, proto)` 単位で **Req/s の median±stdev**、レイテンシ/CPU/メモリの median、
エラー合計を Markdown 表にまとめます。

---

## バイナリ交互 A/B（改修前 vs 改修後）の手順と落とし穴

`profile_ab.sh` は **cargo プロファイル**（lto/codegen-units）の A/B 用である。
**ソース改修の効果**（例: F-158）を測るときは「改修前後の 2 バイナリを交互に走らせる」
別の手順が要る。AGENTS.md の「性能改善は必ず交互 A/B で確認する」はこちらを指す。

### 手順

1. **2 バイナリを別ディレクトリに置く。basename は `veil` のままにする**（F-156）。
   `/root/ab/base/veil` と `/root/ab/new/veil` のように**ディレクトリで分ける**。
   `veil.base` のような別名にすると `pkill -x veil` が 1 つも kill せず、
   `SO_REUSEPORT`(_LB) で旧プロセスが同じポートを掴んだまま残り、
   **新旧混合を計測する**（F-156 で実際に踏んだ）。
2. **計測前に 2 バイナリの md5 が異なることを必ず確認する**（下記の落とし穴 1・2）。
3. **ラウンドごとに実行順を入れ替える**（奇数ラウンドは base→new、偶数は new→base）。
   計測系には時間ドリフトがあるため、順序を固定すると後半の変種が不利になる。
4. **判定は「何ラウンド勝ったか」と「分布が重なるか」で行う**。
   F-156 の教訓どおり、**4 ラウンド方向が揃っただけでは有意ではない**。
   F-158 は 10 ラウンド全勝かつ **base の最大 < new の最小**（完全分離）で確定させた。

### 落とし穴（F-158 で実際に踏んだもの）

| # | 事象 | 対策 |
|---|---|---|
| 1 | **`tools/qemu/bsd-vm.sh <os> <arch> ssh` は stdin を転送しない。** `tar czf - src \| bsd-vm.sh ... ssh "tar xzf -"` が**エラーを出さずに何も展開しない** | VM へのファイル流し込みは直接 `ssh -i ~/.ssh/veil_qemu_key -p <port> root@127.0.0.1` を使う。**ただしビルド実行は迂回しない**（下記 5） |
| 2 | **`git archive HEAD` はファイル mtime をコミット時刻にする。** cargo は mtime で差分判定するため既存フィンガープリントより古いと**再ビルドが走らない** | 展開後に `find src -name '*.rs' -exec touch {} +` |
| 3 | 1+2 の合わせ技で **base と new の md5 が完全一致**したまま A/B を回しかけた（＝「差が無い＝退行なし」という誤結論が出るところだった） | **A/B 開始前に md5 差分を assert する** |
| 4 | **`bsd-vm.sh fetch` はビルドが失敗しても、過去の `target/<profile>/veil` が残っていれば「取得完了」と報告する** | fetch 前に `rm -rf target/<profile>`、または取得後に**バイナリの日付・サイズを VM 側と突き合わせる** |
| 5 | **`CARGO_PROFILE=dist` を `build` にだけ渡して `fetch` に渡し忘れる**と、`release` の成果物を取得してしまう | `build` と `fetch` の**両方**に渡す |

**共通する失敗モードは「成功メッセージを出しながら間違った成果物を作る」こと。**
数字を読む前に、**測っている 2 つが本当に別物か**を毎回確認すること。

### 適用例

F-158（HTTP/2 インライン初回 poll）の A/B 結果は
[F-158 のチケット](../../docs/backlog/features/F-158-h2-inline-first-poll.md) を参照
（`docs/perf` は最新の計測だけを載せるため、過去の A/B の生データは git 履歴にある）。
**同一の変更が kqueue で +27.3%、io_uring で −4.6% と正反対になった**ため、
**ホットパス最適化は必ず両バックエンドで A/B を取ること**。

## 注意

- 4 コア程度のホストや co-tenant 負荷がある環境では計測が揺れます。**quiet host（loadavg 低）** での
  計測と、同一実行内の相対比較を推奨します（負荷フレークに注意）。
- `configs/_debug*.toml` / `results/logs/` / `results/results_raw.tsv` / `results/results_summary.md` は `.gitignore` 対象です。
