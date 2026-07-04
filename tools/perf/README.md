# perf — Veil vs nginx パフォーマンス計測ハーネス

`veil:glibc` / `veil:musl`（full features ビルド）と `nginx:alpine` を **同一ネットワーク上のコンテナ間通信** で計測し、HTTP/1.1（`wrk`）と HTTP/2（`h2load`）のスループット・レイテンシ・CPU/メモリ使用量を TSV に集約するハーネスです。

すべて **docker コマンドのみ** で完結し、ホストへの追加インストールは不要です（証明書生成に `openssl` を使う場合を除く）。

計測結果レポートの例は [docs/artifacts/](../../docs/artifacts/) の `performance_report_veil_vs_nginx*.md` を参照してください。バックログ上は [F-58](../../docs/backlog/features/F-58-perf-report-glibc-musl-nginx.md) の再現ハーネスです。

---

## 構成

| パス | 役割 |
|------|------|
| `run_perf.sh` | 計測オーケストレータ（nginx → veil glibc/musl × 各バリアント） |
| `gen_configs.sh` | 計測用 `config.toml` バリアントを `configs/` に生成 |
| `configs/*.toml` | 生成済みバリアント（`gen_configs.sh` で再生成可能） |
| `nginx/nginx.conf` | 比較対象 nginx の設定（`access_log off` で公平化） |
| `results/` | 計測結果（`results.tsv` / `logs/` は `.gitignore` 対象） |

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

完了すると結果テーブルが標準出力に表示され、`results/results.tsv` に保存されます。個別の負荷ツールログと CPU/メモリのサンプルは `results/logs/` 以下に残ります。

### 負荷パラメータ（`run_perf.sh` 冒頭で調整可能）

| 変数 | 既定 | 意味 |
|------|------|------|
| `WRK_ARGS` | `-t4 -c100 -d15s --timeout 5s --latency` | HTTP/1.1（wrk）: 4 スレッド・100 接続・15 秒 |
| `H2_ARGS` | `-n 30000 -c100 -m10` | HTTP/2（h2load）: 30000 リクエスト・100 接続・多重化 10 |

---

## 計測バリアント（`gen_configs.sh`）

いずれも同一の静的ファイル（`/var/www/index.html`）を `File` アクションで配信し、`http2 / ktls / reuseport balancing / open_file_cache` の組み合わせのみを変えます。アクセスログは `logging.level = "warn"` で抑止し、nginx の `access_log off` と条件を揃えます。

| 名前 | http2 | ktls | reuseport_balancing | open_file_cache |
|------|:-----:|:----:|:-------------------:|:---------------:|
| `base` | ✓ | ✓ | cbpf | – |
| `no_ktls` | ✓ | – | cbpf | – |
| `no_http2` | – | ✓ | cbpf | – |
| `kernel_lb` | ✓ | ✓ | kernel | – |
| `ofc` | ✓ | ✓ | cbpf | ✓ |
| `no_ktls_ofc` | –→✓ | – | cbpf | ✓ |

---

## 出力フォーマット（`results/results.tsv`）

タブ区切りで 1 計測 1 行。列は以下のとおり。

```
target  config  proto   req_per_sec  transfer  lat_avg  lat_p99  non2xx  cpu_pct  mem_mb
```

- `target`: `nginx` / `veil_glibc` / `veil_musl`
- `config`: バリアント名（nginx は `base` 固定）
- `proto`: `http1.1`（wrk）/ `http2`（h2load）
- `cpu_pct` / `mem_mb`: 負荷中に `docker stats` を 3 回サンプルした平均

---

## 注意

- 4 コア程度のホストや co-tenant 負荷がある環境では計測が揺れます。比較は同一実行内の相対値で評価してください（[docs/artifacts/](../../docs/artifacts/) の負荷フレーキー記録参照）。
- `configs/_debug*.toml` / `results/logs/` / `results/results.tsv` は `.gitignore` 対象です。
