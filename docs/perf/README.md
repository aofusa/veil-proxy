# docs/perf — パフォーマンス計測結果

Veil の HTTP/1.1・HTTP/2 スループット／レイテンシ／CPU・メモリ使用量を、`nginx:alpine`
を基準に **同一 Docker ネットワーク上のコンテナ間通信** で計測した結果を記録する。

計測ハーネスは [`tools/perf/`](../../tools/perf/) を参照（`gen_configs.sh` で構成生成 /
`run_perf.sh` で反復計測 / `analyze_results.sh` で median±stdev 集計）。

- 完全なデータ（1 反復 1 行の生データ）: [`results_raw.tsv`](results_raw.tsv)
- 集計サマリ（median±stdev）: [`results_summary.md`](results_summary.md)
- HTTP/3・gRPC・WebSocket 拡張計測 + F-106/F-111 最適化: [`protocol_extended_results.md`](protocol_extended_results.md)
- 過去の計測レポートの要約・時系列: [`history.md`](history.md)

---

## 計測対象（default features と full features の機能面）

Veil のリリースバイナリは **full features**（`--build-arg CARGO_FEATURES='full'`）で
ビルドしたコンテナイメージ `veil:glibc` / `veil:musl` を用いる。default features
（`ktls` + `http2` + `mimalloc`）で入るプロトコル面に加え、**full でのみ有効化される
機能**（compression / cache / buffering / 逆プロキシ経路）についてもスループットを計測する。

| 構成名 | 内容 | 主に測る機能 |
|--------|------|--------------|
| `h2_1_ktls_1_lb_kernel_ofc_1` | 静的ファイル配信・HTTP/2・kTLS・kernel LB・open_file_cache | default+http2 の最良チューニング基準 |
| `h2_1_ktls_0_lb_cbpf_ofc_0` | 静的ファイル配信・HTTP/2・rustls（kTLS 無効）・CBPF LB | ユーザ空間 TLS 基準 |
| `h2_1_feat_compression` | 静的ファイル + レスポンス圧縮（zstd/br/gzip、Accept-Encoding 付与） | **compression** |
| `h2_1_feat_cache` | 静的ファイル + インメモリキャッシュ（GET/200） | **cache** |
| `h2_1_feat_proxy` | 逆プロキシ（上流 nginx へ中継） | **Proxy ホットパス** |
| `h2_1_feat_buffering` | 逆プロキシ + 高度なバッファリング制御（full） | **buffering** |

> 完全直交 2⁴=16 の静的配信バリアント（http2 × ktls × LB × open_file_cache）は
> `tools/perf/gen_configs.sh` で生成できる。本計測（2026-07-07）は **全 16 静的バリアント +
> full 機能 4 点** を実行した（上表は代表構成の抜粋。全結果は
> [`results_summary.md`](results_summary.md) 参照）。

---

## 計測条件

| 項目 | 値 |
|------|-----|
| ハードウェア | 4 コア（quiet host・計測開始時 loadavg < 0.5 で取得） |
| HTTP/1.1 負荷 | `wrk -t4 -c100 -d10s`（ウォームアップ後 1 反復） |
| HTTP/2 負荷 | `h2load -n30000 -c100 -m10`（ウォームアップ後 1 反復） |
| TLS | 自己署名 ECDSA(secp384r1)、TLS1.3 |
| 配信コンテンツ | `docker/assets/www/index.html`（約 54KB・圧縮対象として十分なサイズ） |
| 基準 | `nginx:alpine`（`access_log off`・HTTP/2 有効で公平化） |

計測はコンテナ間通信のため絶対値はホスト直計測より低めに出るが、**同一条件での相対比較**
（veil vs nginx、機能有無、glibc vs musl）が目的である。

---

## 結果サマリ

完全なデータは [`results_raw.tsv`](results_raw.tsv)（1 反復 1 行）、集計は
[`results_summary.md`](results_summary.md)（median±stdev）に格納する。以下は
2026-07-07（v0.5.0、B-25/B-27/B-28 修正後）の計測（全 2⁴=16 静的バリアント + full 機能
4 点・wrk 10s・h2load 30k req・quiet host loadavg<0.5）の要点。

### HTTP/1.1（wrk -t4 -c100 -d10s、req/s median）

| 構成 | nginx | veil:glibc | veil:musl |
|------|-------|-----------|-----------|
| 静的配信・最良（h2on/rustls/kernel LB/OFC） | 2309 | **3124** | **3178** |
| 静的配信・kTLS/kernel LB/OFC | — | 2494 | 2464 |
| 静的配信・rustls（CBPF LB） | — | 1463 | 1470 |
| cache（インメモリ・GET/200） | — | 2157 | 2167 |
| compression（zstd/br/gzip・54KB） | — | 2114 | 2128 |
| proxy（逆プロキシ・ストリーミング） | — | 1763 | 1779 |
| buffering（逆プロキシ・full バッファ） | — | 1828 | 1775 |

### HTTP/2（h2load -n30000 -c100 -m10、req/s median）

| 構成 | nginx | veil:glibc | veil:musl |
|------|-------|-----------|-----------|
| 静的配信・最良（rustls/kernel LB/OFC） | 2435 | **2757** | **2685** |
| 静的配信・kTLS/kernel LB/OFC | — | 2513 | 2490 |
| cache（インメモリ・GET/200） | — | 2473 | 2461 |
| compression（zstd/br/gzip・54KB） | — | 2530 | 2495 |
| proxy（逆プロキシ・ストリーミング） | — | 1985 | 2042 |
| buffering（逆プロキシ・full） | — | 2038 | 2021 |

### 観測

- **静的配信の最良チューニング**（rustls + kernel SO_REUSEPORT + open_file_cache）で
  veil は nginx を明確に上回る（HTTP/1.1 +35〜38%、HTTP/2 +10〜13%）。
  **エラーは全 68 計測で 0**。
- コンテナ（veth）では kTLS 有効が rustls 比で不利（HTTP/1.1 3124→2494 等）。
  単一クライアント IP 負荷では CBPF LB が 1 ワーカーに集約されるため kernel LB が有利。
- **full 限定機能**（compression / cache / buffering / 逆プロキシ）はいずれも安定して
  計測でき、機能有効時のオーバーヘッドが可視化されている。
- glibc / musl は概ね同等（拮抗、構成によりわずかに前後）。
- **修正履歴**: 2026-07-06 の初回計測で検出された 3 つの異常は v0.5.0 で修正済み。
  1. `feat_proxy` HTTP/1.1 の「wrk 完了リクエスト 0」→ kTLS splice の `SPLICE_F_MORE`
     による最終部分 TLS レコード保留（[B-25](../backlog/bugs/B-25-reverse-proxy-http1-wrk-zero-completed.md)）。
  2. kTLS + HTTP/2 高並行の激減（256〜736 req/s）→ `write_all` の short write 未継続に
     よるフレーム同期破壊（[B-27](../backlog/bugs/B-27-ktls-http2-short-write-frame-desync.md)）。
  3. HTTP/2 逆プロキシの 5xx 混入（h2load 30k 中 1000〜1500 件）→ バックエンド接続
     非再利用によるエフェメラルポート枯渇（[B-28](../backlog/bugs/B-28-h2-proxy-no-backend-pooling-port-exhaustion.md)）。
- 絶対値はコンテナ間通信のため実ホストより低めに出る点に留意。

---

## 再現手順

```bash
# 1) full features でコンテナイメージをビルド（docker/README.md 参照）
docker build -f docker/Dockerfile.glibc -t veil:glibc --build-arg CARGO_FEATURES='full' .
docker build -f docker/Dockerfile.musl  -t veil:musl  --build-arg CARGO_FEATURES='full' .

# 2) 計測用 config を生成（16 静的 + 4 full 機能）
bash tools/perf/gen_configs.sh

# 3) 計測（nginx → veil glibc/musl × 各構成 × 反復）。完了後に集計も実行される
bash tools/perf/run_perf.sh

# 生データ:   tools/perf/results/results_raw.tsv
# 集計サマリ: tools/perf/results/results_summary.md
```

---

## 過去の計測レポート・アーカイブ

個別の計測レポート（Veil vs nginx 各版、ECDSA/並列メトリクス、full features 機能別
オーバーヘッド、`tools/perf` 実装検証など）の要約は [`history.md`](history.md) を参照。
生の詳細レポートはリポジトリ肥大化を避けるため `docs/artifacts/perf_reports/`
（git 管理外の AI 作業用スクラッチ）へ退避しており、本リポジトリの追跡対象には含まない。
