# perf — Veil vs nginx パフォーマンス計測ハーネス

`veil:glibc` / `veil:musl`（full features ビルド）と `nginx:alpine` を **同一ネットワーク上のコンテナ間通信** で計測し、HTTP/1.1（`wrk`）と HTTP/2（`h2load`）のスループット・レイテンシ・CPU/メモリ使用量を TSV に集約するハーネスです。

すべて **docker コマンドのみ** で完結し、ホストへの追加インストールは不要です（証明書生成に `openssl` を使う場合を除く）。

計測結果は [docs/perf/](../../docs/perf/) を参照してください。バックログ上は [F-58](../../docs/backlog/features/F-58-perf-report-glibc-musl-nginx.md) の再現ハーネスです。

---

## 構成

| パス | 役割 |
|------|------|
| `run_perf.sh` | 計測オーケストレータ（nginx → veil glibc/musl × 全バリアント × 反復）。完了後に集計も実行 |
| `gen_configs.sh` | 計測用 `config.toml` バリアントを **完全直交 2⁴=16** で `configs/` に生成 |
| `analyze_results.sh` | 反復生データ（`results_raw.tsv`）を **median±stdev** に集計し Markdown を出力 |
| `configs/*.toml` | 生成済みバリアント（`gen_configs.sh` で再生成可能） |
| `nginx/nginx.conf` | 比較対象 nginx の設定（`access_log off` で公平化） |
| `results/` | 計測結果（`results_raw.tsv` / `results_summary.md` / `logs/` は `.gitignore` 対象） |

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
| TLS 証明書 | `docker/assets/ssl/` に自己署名証明書（[docker/README.md](../../docker/README.md) の openssl 手順で生成） |

> `local/h2load:latest` は環境に h2load を含むイメージがない場合、`nghttp2` を含む任意の Dockerfile から `docker build -t local/h2load:latest .` で用意してください（`--entrypoint h2load` で起動します）。

---

## 実行方法

```sh
# 1) 計測用 config バリアントを生成（configs/*.toml を再生成）
bash tools/perf/gen_configs.sh

# 2) 計測を実行（nginx → veil:glibc → veil:musl × 各バリアント）
bash tools/perf/run_perf.sh
```

リポジトリのどこから実行しても、スクリプトが自身の位置からリポジトリルートと `docker/assets/` を解決します。

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

> 全 16 構成 × glibc/musl × 反復 のフルスイートは時間がかかります。素早く確認したい場合は
> `ITERATIONS=1` や、`configs/` を一部だけ残して実行してください。

---

## 計測バリアント（`gen_configs.sh`）

いずれも同一の静的ファイル（`/var/www/index.html`）を `File` アクションで配信し、4 因子
**`http2 × ktls × reuseport_balancing(cbpf/kernel) × open_file_cache`** を **完全直交（2⁴=16 構成）**
で組み合わせます。アクセスログは `logging.level = "warn"` で抑止し、nginx の `access_log off` と条件を揃えます。

バリアント名は `h2_<0|1>_ktls_<0|1>_lb_<cbpf|kernel>_ofc_<0|1>`。`run_perf.sh` は名前の `h2_1` から
HTTP/2 負荷（h2load）の要否を判定します（`h2_0_*` は wrk のみ）。

主な着目点（[docs/perf/results_summary.md](../../docs/perf/results_summary.md) 参照、2026-07-07 v0.5.0 計測）:

- **最良構成 `h2_1_ktls_0_lb_kernel_ofc_1`**（HTTP/2 有効・kTLS 無効・kernel LB・OFC 有効）で
  **veil は nginx を上回る**（HTTP/1.1 glibc 3124 / musl 3178 vs nginx 2309 = +35〜38%、
  HTTP/2 glibc 2757 / musl 2685 vs nginx 2435 = +10〜13%）。エラーは全 68 計測で 0。
- コンテナ（veth）では **kTLS 有効が不利**（`ktls_1` は rustls 比で低下）。
- 単一クライアント IP 負荷では **`cbpf` が 1 ワーカーに集約**して 4 コアを使い切れず、`kernel` 分散が有利。
- 過去計測（2026-07-06）で異常だった「`feat_proxy` HTTP/1.1 の wrk 完了 0」「`kernel` +
  HTTP/2 + `ktls_1` の激減」「HTTP/2 逆プロキシの 5xx 混入」は、それぞれ
  B-25（splice `SPLICE_F_MORE`）/ B-27（`write_all` short write）/ B-28（バックエンド接続
  プーリング欠如）として **v0.5.0 で修正済み**（[docs/backlog/backlog.md](../../docs/backlog/backlog.md) 参照）。

---

## 出力フォーマット

### 生データ `results/results_raw.tsv`（1 反復 1 行）

```
target  config  proto  iteration  req_per_sec  transfer  lat_avg  lat_p99  non2xx  cpu_pct  mem_mb
```

- `target`: `nginx` / `veil_glibc` / `veil_musl`
- `config`: バリアント名（nginx は `base` 固定）
- `proto`: `http1.1`（wrk）/ `http2`（h2load）
- `iteration`: 反復番号（1..`ITERATIONS`）
- `cpu_pct` / `mem_mb`: 各反復の負荷中に `docker stats` を 3 回サンプルした平均

### 集計 `results/results_summary.md`（`analyze_results.sh`）

`(target, config, proto)` 単位で **Req/s の median±stdev**、レイテンシ/CPU/メモリの median、
エラー合計を Markdown 表にまとめます。

---

## 注意

- 4 コア程度のホストや co-tenant 負荷がある環境では計測が揺れます。**quiet host（loadavg 低）** での
  計測と、同一実行内の相対比較を推奨します（負荷フレークに注意）。
- `configs/_debug*.toml` / `results/logs/` / `results/results_raw.tsv` / `results/results_summary.md` は `.gitignore` 対象です。
