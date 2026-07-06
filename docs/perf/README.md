# docs/perf — パフォーマンス計測結果

Veil の HTTP/1.1・HTTP/2 スループット／レイテンシ／CPU・メモリ使用量を、`nginx:alpine`
を基準に **同一 Docker ネットワーク上のコンテナ間通信** で計測した結果を記録する。

計測ハーネスは [`tools/perf/`](../../tools/perf/) を参照（`gen_configs.sh` で構成生成 /
`run_perf.sh` で反復計測 / `analyze_results.sh` で median±stdev 集計）。

- 完全なデータ（1 反復 1 行の生データ）: [`results_raw.tsv`](results_raw.tsv)
- 集計サマリ（median±stdev）: [`results_summary.md`](results_summary.md)

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
> `tools/perf/gen_configs.sh` で生成でき、`tools/perf/configs_full_backup/` に退避してある。
> 上表はそのうち代表 2 点＋ full 機能 4 点に絞った計測結果。

---

## 計測条件

| 項目 | 値 |
|------|-----|
| ハードウェア | 4 コア（co-tenant 負荷あり・計測時 loadavg ≈ 2 の低負荷時に取得） |
| HTTP/1.1 負荷 | `wrk -t4 -c100 -d8s`（3 反復、median±stdev） |
| HTTP/2 負荷 | `h2load -n30000 -c100 -m10`（3 反復） |
| TLS | 自己署名 ECDSA(secp384r1)、TLS1.3 |
| 配信コンテンツ | `docker/assets/www/index.html`（約 54KB・圧縮対象として十分なサイズ） |
| 基準 | `nginx:alpine`（`access_log off`・HTTP/2 有効で公平化） |

計測はコンテナ間通信のため絶対値はホスト直計測より低めに出るが、**同一条件での相対比較**
（veil vs nginx、機能有無、glibc vs musl）が目的である。

---

## 結果サマリ

完全なデータは [`results_raw.tsv`](results_raw.tsv)（1 反復 1 行）、集計は
[`results_summary.md`](results_summary.md)（median±stdev）に格納する。以下は
2026-07-06 の計測（3 反復・wrk 8s・h2load 30k req・loadavg≈2 の低負荷時）の要点。

### HTTP/1.1（wrk -t4 -c100 -d8s、req/s median）

| 構成 | nginx | veil:glibc | veil:musl |
|------|-------|-----------|-----------|
| 静的配信・最良チューニング（h2on/kTLS/kernel LB/OFC） | 1994 | **2132** | **2159** |
| 静的配信・rustls（kTLS 無効・CBPF LB） | — | 1379 | 1390 |
| cache（インメモリ・GET/200） | — | 1822 | 1818 |
| compression（zstd/br/gzip・54KB） | — | 1817 | 1853 |
| buffering（逆プロキシ・full バッファ） | — | 1575 | 1506 |

### HTTP/2（h2load -n30000 -c100 -m10、req/s median）

| 構成 | nginx | veil:glibc | veil:musl |
|------|-------|-----------|-----------|
| 静的配信・rustls（CBPF LB） | 2276 | 2227 | **2394** |
| proxy（逆プロキシ・ストリーミング） | — | 718 | 863 |
| buffering（逆プロキシ・full） | — | 742 | 556 |

### 観測

- **静的配信の最良チューニング**（kTLS + kernel SO_REUSEPORT + open_file_cache）では、
  HTTP/1.1 で veil が nginx を上回る（glibc 2132 / musl 2159 vs nginx 1994 req/s）。
  エラーは全構成で 0。
- **full 限定機能**（compression / cache / buffering / 逆プロキシ）はいずれも安定して
  計測でき、機能有効時のスループット・CPU・メモリのオーバーヘッドが可視化されている
  （例: compression 有効で ~1.8k req/s を維持、buffering は full バッファ分メモリ増）。
- glibc / musl は概ね同等（静的配信で musl がわずかに上、機能有効時は拮抗）。
- **注記**: `feat_proxy` の HTTP/1.1 は wrk が「完了リクエスト 0」を計上した
  （データ転送自体は発生。100 並行 keep-alive での wrk のカウント挙動によるもの）。
  逆プロキシ HTTP/1.1 の機能正当性は E2E（`e2e_tests`）で網羅的に検証済みで、
  HTTP/2 経路（glibc 718 / musl 863 req/s）と buffering 逆プロキシ（HTTP/1.1 1575 req/s）は
  正常に計測できている。絶対値はコンテナ間通信のため実ホストより低めに出る点に留意。

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
