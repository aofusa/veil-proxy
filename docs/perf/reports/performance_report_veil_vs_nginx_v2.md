# Veil Proxy パフォーマンス計測レポート v2（glibc / musl / nginx 比較）

本レポートは、**B-13（seccomp faccessat2）/ B-14（cache 無効時の 404）/ B-15（Dockerfile ビルド）**
の修正後に、Veil Proxy（glibc 版・musl 版）と nginx(alpine) をコンテナ間通信で比較計測した結果である。
旧レポート（`performance_report_veil_vs_nginx.md`）は修正前の測定で HTTP/1.1・musl 版が全滅していたため、
本 v2 で再測定した。

> **前提となる修正（本ブランチ `feat/perf-musl-http1-fixes`）**
> - B-13: veil の seccomp 許可リスト（および `docker/assets/security/seccomp.json`）に
>   `faccessat2`(439) が無く、seccomp 有効時にファイル解決が EPERM で失敗して静的配信が
>   404 になっていた（HTTP/1.1 全滅・musl 版配信不能の主因）。→ 許可リストへ追加して解消。
> - B-14: `cache` feature 無効時に `get_file_info` がスタブで `None` を返し静的配信が 404。→ 実解決へ修正。
> - B-15: Dockerfile が fuzz ワークスペースメンバ・lib+bin 構成・同梱 config.toml に追随できず
>   イメージがビルド不能。→ 修正しビルド可能化。

## 1. 測定環境・条件

- **ホスト**: Linux x86_64, 4 論理コア, 11 GiB RAM。
- **ネットワーク**: Docker カスタムネットワーク `perf_net` を用いたコンテナ間通信。
- **TLS 証明書**: 自己署名 ECDSA (`secp384r1`)、`docker/assets/ssl/`（SAN: localhost, 127.0.0.1）。
- **ペイロード**: `docker/assets/www/index.html`（約 650 KB の静的 HTML、3 者で同一）。
- **ロードツール（コンテナ）**:
  - HTTP/1.1: `williamyeh/wrk`（`-t4 -c100 -d15s --latency`）
  - HTTP/2: `h2load`（nghttp2, `-n30000 -c100 -m10`）
- **計測対象コンテナ**:
  - `nginx:alpine`（ベースライン、access_log off で公平化）
  - `veil:glibc` / `veil:musl`（`CARGO_FEATURES='ktls,http2,mimalloc,cache'` でビルド）
- **公平化**: Veil は `logging.level = "warn"`（アクセスログ抑止）、nginx は `access_log off`。
  いずれもデータプレーン（TLS 終端 + 静的配信）そのものを測る。
- **追加指標**: `docker stats` を負荷中に 3 回サンプリングして CPU%・メモリ使用量（MiB）を平均。
  wrk は `--latency` で p99 レイテンシも取得。

> **feature 構成について**: 対象イメージは `ktls,http2,mimalloc,cache` でビルドしている。
> 本計測で変化させる設定（kTLS / HTTP/2 / reuseport balancing / OpenFileCache）はこの feature
> 集合で効果が出るものに限定した（wasm/http3/grpc 等は本計測の対象外）。

## 2. 測定対象の設定組み合わせ（config バリアント）

いずれも同一の静的ファイル（`/var/www/index.html`）を `File` アクションで配信し、
`seccomp=filter` + Landlock 有効の実運用相当のセキュア構成で計測する。

| バリアント | http2 | kTLS | reuseport balancing | OpenFileCache | 意図 |
|-----------|-------|------|---------------------|---------------|------|
| `base`        | on  | on  | cbpf   | off | すべての最適化を有効化した基本構成 |
| `no_ktls`     | on  | off | cbpf   | off | kTLS を無効化しユーザー空間 rustls のみ |
| `no_http2`    | off | on  | cbpf   | off | HTTP/1.1 のみ（HTTP/2 無効） |
| `kernel_lb`   | on  | on  | kernel | off | reuseport をカーネル既定分散に変更 |
| `ofc`         | on  | on  | cbpf   | on  | OpenFileCache でファイルメタデータ syscall を削減 |
| `no_ktls_ofc` | on  | off | cbpf   | on  | rustls + OpenFileCache の複合 |

（生成スクリプト: `tools/perf/gen_configs.py`、ハーネス: `tools/perf/run_perf.sh`）

## 3. 測定結果

計測時のホスト負荷は low（loadavg ≈ 1.4、他の重い処理なし）。**全 24 計測で Non-2xx = 0**
（旧レポートで全滅していた HTTP/1.1・musl 版が正常配信されるようになった）。
Transfer は wrk が `Transfer/sec`、h2load が `MB/s`（いずれも毎秒値）。CPU% は 4 コアなので
最大 400%。h2load の p99 は取得対象外（NA）。

### 3.1 ベースライン（nginx:alpine）

| proto | Req/sec | Transfer/s | Lat avg | Lat p99 | Non-2xx | CPU% | Mem MB |
|-------|--------:|-----------:|--------:|--------:|:-------:|-----:|-------:|
| HTTP/1.1 | **1768.4** | 92.5 MB | 54.8ms | 131.0ms | 0 | 206 | 21.9 |
| HTTP/2   | **1931.8** | 100.9 MB | 386.1ms | – | 0 | 124 | 26.3 |

### 3.2 Veil glibc

| config | proto | Req/sec | Transfer/s | Lat avg | Lat p99 | Non-2xx | CPU% | Mem MB |
|--------|-------|--------:|-----------:|--------:|--------:|:-------:|-----:|-------:|
| base | HTTP/1.1 | 960.7 | 50.1 MB | 101.2ms | 189.0ms | 0 | 102 | 25.5 |
| base | HTTP/2 | 924.0 | 48.6 MB | 616.5ms | – | 0 | 69 | 42.4 |
| no_ktls | HTTP/1.1 | 1305.2 | 68.1 MB | 74.8ms | 163.1ms | 0 | 102 | 31.9 |
| no_ktls | HTTP/2 | 1569.0 | 81.7 MB | 471.3ms | – | 0 | 68 | 46.1 |
| no_http2 | HTTP/1.1 | 951.3 | 49.6 MB | 102.6ms | 191.5ms | 0 | 103 | 25.2 |
| kernel_lb | HTTP/1.1 | **1649.9** | 86.1 MB | 59.4ms | 151.9ms | 0 | 190 | 29.5 |
| kernel_lb | HTTP/2 | **166.4** | 33.7 MB | 358.0ms | – | 0 | 32 | 48.4 |
| ofc | HTTP/1.1 | 1132.3 | 59.1 MB | 85.8ms | 106.2ms | 0 | 101 | 26.4 |
| ofc | HTTP/2 | 937.1 | 49.1 MB | 638.4ms | – | 0 | 67 | 44.4 |
| **no_ktls_ofc** | HTTP/1.1 | **1664.5** | 86.8 MB | 56.8ms | 90.2ms | 0 | 102 | 33.9 |
| **no_ktls_ofc** | HTTP/2 | **1530.4** | 79.7 MB | 463.4ms | – | 0 | 68 | 47.8 |

### 3.3 Veil musl

| config | proto | Req/sec | Transfer/s | Lat avg | Lat p99 | Non-2xx | CPU% | Mem MB |
|--------|-------|--------:|-----------:|--------:|--------:|:-------:|-----:|-------:|
| base | HTTP/1.1 | 931.6 | 48.6 MB | 102.6ms | 193.3ms | 0 | 101 | 24.7 |
| base | HTTP/2 | 941.5 | 49.3 MB | 780.8ms | – | 0 | 66 | 43.2 |
| no_ktls | HTTP/1.1 | 1270.5 | 66.3 MB | 75.8ms | 169.9ms | 0 | 103 | 43.4 |
| no_ktls | HTTP/2 | 1467.2 | 76.4 MB | 479.7ms | – | 0 | 59 | 57.0 |
| no_http2 | HTTP/1.1 | 947.1 | 49.4 MB | 102.6ms | 194.3ms | 0 | 103 | 26.6 |
| kernel_lb | HTTP/1.1 | **1641.8** | 85.7 MB | 59.5ms | 154.3ms | 0 | 194 | 30.6 |
| kernel_lb | HTTP/2 | **168.7** | 34.7 MB | 356.2ms | – | 0 | 24 | 54.4 |
| ofc | HTTP/1.1 | 1138.9 | 59.4 MB | 85.2ms | 108.9ms | 0 | 102 | 36.6 |
| ofc | HTTP/2 | 914.6 | 47.9 MB | 632.2ms | – | 0 | 68 | 54.8 |
| **no_ktls_ofc** | HTTP/1.1 | **1675.3** | 87.4 MB | 57.8ms | 85.8ms | 0 | 102 | 42.8 |
| **no_ktls_ofc** | HTTP/2 | **1448.6** | 75.5 MB | 493.5ms | – | 0 | 68 | 58.3 |

（生データ: [perf_results_v2_raw.tsv](perf_results_v2_raw.tsv)）

## 4. 考察

### 4.1 修正の効果（最重要）
旧レポートでは Veil の HTTP/1.1 が全て Non-2xx、musl 版は配信不能だったが、**B-13
（seccomp に `open`/`faccessat2` 追加）**・**B-14（cache 無効時のファイル解決）** の修正で
**全構成・全プロトコル・glibc/musl 双方が Non-2xx = 0** で正常配信されるようになった。
スループットも旧レポートの数十 KB/s（エラーページ）から **数十 MB/s（実ファイル配信）** へ回復。

### 4.2 kTLS はコンテナ環境ではオーバーヘッドになる
`base`（kTLS 有効）と `no_ktls`（rustls のみ）を比べると、**kTLS 無効の方が明確に速い**:
- glibc HTTP/1.1: 960 → 1305 req/s（+36%）、HTTP/2: 924 → 1569 req/s（+70%）
- musl も同様の傾向。

コンテナの veth / ループバック経路では、kTLS のカーネルオフロードよりもユーザー空間
rustls の方がコンテキストスイッチ・コピー経路が短く有利。旧レポートの指摘を実測で追認した。
**コンテナ運用では `ktls_enabled = false` を推奨。**

### 4.3 reuseport balancing: cbpf は単一クライアント IP を 1 ワーカーに集約する
`base`（cbpf）と `kernel_lb`（kernel）で HTTP/1.1 が大きく変わる:
- glibc HTTP/1.1: cbpf 960 → kernel **1650** req/s（+72%）

`cbpf` は **クライアント IP ハッシュ** で接続をワーカーに固定するため、本ベンチのように
**負荷生成側が単一コンテナ（単一 IP）** だと全接続が 1 ワーカーに集中し、4 コアを使い切れない
（CPU も base ≈ 102% ≈ 1 コア）。`kernel` 分散は IP に依らず接続を全ワーカーへ広げるため
CPU 190%（≈2 コア）まで使え高スループット。
**実運用（多数のクライアント IP）では cbpf でも自然に分散する**点に注意。ベンチ結果は
「単一 IP 負荷での worker affinity の影響」を表している。

### 4.4 ただし kernel 分散は HTTP/2 を著しく劣化させる
`kernel_lb` の HTTP/2 は **166 req/s** と激減（base 924 の 1/5）。HTTP/2 は少数の長寿命接続に
多重ストリームを載せるため、kernel の接続分散と相性が悪く（接続が偏る／再分散が効かない）、
特定ワーカーに偏って詰まる。**HTTP/2 主体なら cbpf、HTTP/1.1 主体で単一 LB 経由なら kernel** と、
ワークロードで選ぶべき。両立を狙うなら cbpf が無難。

### 4.5 OpenFileCache は静的配信の HTTP/1.1 を底上げ
`ofc`（base + OpenFileCache）は HTTP/1.1 を 960 → 1132 req/s（+18%、glibc）に改善。
canonicalize/metadata の syscall 削減が効いている。`no_ktls_ofc`（kTLS 無効 + OFC）が
**Veil の最良構成**で、HTTP/1.1 1664 / HTTP/2 1530 req/s（glibc）と nginx に肉薄する。

### 4.6 glibc 版 と musl 版はほぼ同等
同一構成での glibc/musl 差は数 %以内で、優劣は構成により入れ替わる程度。musl 版はイメージが
**約 10.6 MB**（glibc 版 24.8 MB、nginx:alpine 62 MB）と最小で、性能を落とさず配布サイズを
削減できる。メモリ使用は musl の方がやや多い傾向（アロケータ挙動差）。

### 4.7 nginx との比較と CPU 効率
絶対スループットは nginx が最速（HTTP/1.1 1768、HTTP/2 1932 req/s）。一方 Veil は最良構成で
HTTP/1.1 1664〜1675（nginx 比 94%）、HTTP/2 1448〜1530（同 75〜79%）に到達。
注目は **CPU 効率** で、nginx が HTTP/1.1 で 206%（≈2 コア）を使うのに対し Veil は多くの構成で
約 100%（≈1 コア）に収まり、**同等スループットをより少ない CPU で** 出す構成もある
（例: glibc no_ktls_ofc は 1664 req/s を CPU 102% で達成）。Thread-per-core + io_uring の
低オーバーヘッドが効いている。ルーティング/非同期経路のさらなる最適化余地はあるが、
**セキュア構成（seccomp+Landlock+kTLS 可）を保ったまま nginx に匹敵する水準** に達している。

## 5. 推奨構成（コンテナ運用）

| 目的 | 推奨 |
|------|------|
| 総合バランス（HTTP/1.1・HTTP/2 両対応） | `ktls_enabled=false` + `reuseport_balancing="cbpf"` + OpenFileCache 有効 |
| HTTP/1.1 スループット最優先（単一 LB 前段） | 上記の `reuseport_balancing="kernel"` |
| 配布サイズ最小 | musl 版（性能はほぼ同等、イメージ 10.6 MB） |

> 注: `ktls_enabled=false` 推奨はあくまで **コンテナ（veth）環境** の話。ベアメタル／
> 大きなレスポンスでカーネルゼロコピー（sendfile+kTLS）が効く環境では再評価が必要。
