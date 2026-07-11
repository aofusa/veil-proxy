# Veil Proxy Performance Benchmark Report (Updated)

This document outlines the performance comparison between **Veil Proxy (glibc and musl builds)** and **Nginx (alpine)**, using the updated `config.toml` containing the `Landlock` read-paths fix. The benchmark was conducted within a containerized environment (Docker internal network) to simulate real-world microservice conditions. 

*注: 「httpへの接続は強制的にhttpsにリダイレクトされる」というご指摘に従い、すべてのベンチマーククライアント（wrk, h2load）は直接 `https://` エンドポイント（port 443）を指定して実行しています。*

## 1. 測定環境・条件 (Benchmark Environment & Conditions)

- **ネットワーク:** Dockerカスタムネットワーク (`perf_net`) を使用したコンテナ間通信。
- **TLS証明書:** 自己署名 ECDSA 証明書 (`secp384r1`)。
- **ペイロード:** `docker/assets/www/index.html` (約650KBの静的ファイル)。
- **ツール:** 
  - **HTTP/1.1:** `wrk` (4スレッド, 100同時接続, 10秒間)
  - **HTTP/2:** `h2load` (10,000リクエスト, 100同時接続)
- **対象コンテナ:**
  - `nginx:alpine` (ベースライン)
  - `veil:glibc` (デフォルトビルド)
  - `veil:musl` (Alpine向け軽量ビルド)

## 2. 測定対象の組み合わせ (Configurations)

Veilコンテナは以下のバリエーションを設定ファイル(`config.toml`)として用意し検証しました。

1. **base**: すべての最適化機能 (kTLS, HTTP/2, CBPF Load Balancing) を有効化した基本構成。
2. **no_ktls**: kTLSを無効化し、ユーザー空間の `rustls` だけでTLS処理を行う構成。
3. **no_http2**: HTTP/2を無効化 (HTTP/1.1のみ)。
4. **kernel_lb**: SO_REUSEPORTのバランシングを `cbpf` (クライアントIPベース) からカーネルのデフォルト設定 (`kernel`) に変更した構成。
5. **ofc (OpenFileCache)**: `open_file_cache_enabled = true` を設定し、ファイルメタデータのシステムコール削減効果を狙った構成。

---

## 3. 測定結果 (Benchmark Results)

### スループット比較マトリクス

| Target | HTTP/1.1 (wrk) Req/sec | HTTP/1.1 (wrk) Transfer/sec | HTTP/2 (h2load) Req/sec | HTTP/2 (h2load) Throughput | Errors (Non-2xx) |
|---|---|---|---|---|---|
| **nginx** | 173.27 | 110.19 MB/s | 142.99 | 90.83 MB/s | 0 |
| **veil_glibc_base** | 767.61 | 47.98 KB/s | 98.10 | 62.27 MB/s | 7735 |
| **veil_glibc_kernel_lb** | 1095.54 | 68.47 KB/s | 0.00 | 74.89 MB/s | 11066 |
| **veil_glibc_no_http2** | 891.12 | 55.70 KB/s | N/A | N/A | 9000 |
| **veil_glibc_no_ktls** | 958.96 | 59.94 KB/s | 165.22 | 104.88 MB/s | 9656 |
| **veil_glibc_ofc** | 885.69 | 55.36 KB/s | 101.88 | 64.82 MB/s | 8899 |
| **veil_musl_base** | 851.97 | 53.25 KB/s | 2837.45 | 79.44 KB/s | 8602 |
| **veil_musl_kernel_lb** | 1093.84 | 68.37 KB/s | 3882.60 | 108.71 KB/s | 11060 |
| **veil_musl_no_http2** | 870.59 | 54.41 KB/s | N/A | N/A | 8791 |
| **veil_musl_no_ktls** | 984.28 | 61.52 KB/s | 3402.14 | 95.25 KB/s | 9938 |
| **veil_musl_ofc** | 862.01 | 53.88 KB/s | 2889.98 | 80.91 KB/s | 8711 |

*(※注: 修正された設定を用いた結果でも、VeilにおけるHTTP/1.1リクエスト (wrk) およびHTTP/2の `veil_musl` は、依然として静的ファイルが配信されずHTTPエラーレスポンス (404 または 403) を返却しています。これについては後述の考察で分析しています)*

---

## 4. 結果の考察と分析 (Analysis)

### 4.1. HTTP/1.1 リクエストの異常 (Non-2xx)
前回の測定同様、すべてのVeil環境において `wrk` (HTTP/1.1) での通信が100%エラー（Non-2xx）として処理されました。
HTTPリダイレクト設定（`http = "0.0.0.0:80"`）を考慮し、クライアントから直接 `https://...` (ポート443) へ接続を行っていますが、それでも 404 Not Found (または 403) エラーとして返却されています。一方で、**同じポート443の同一URLに対するHTTP/2 (`h2load`) リクエストは正常に処理されている (glibc版のみ)** ため、HTTP/1.1パーサーやルーティング評価ロジックに固有の問題が潜んでいると考えられます。

### 4.2. kTLSとrustlsのパフォーマンス逆転現象
`veil_glibc_base` (kTLS有効) と `veil_glibc_no_ktls` (rustlsのみ) のHTTP/2スループットを比較すると、前回の検証と同様の結果が見られました。
- **kTLS有効:** 62.27 MB/s (98.10 req/s)
- **kTLS無効 (rustls):** 104.88 MB/s (165.22 req/s)

Dockerコンテナ環境においては、kTLSによるカーネルオフロードよりも純粋なユーザー空間処理(rustls)の方が大幅に高速(約1.6倍)であることが再確認されました。これはコンテナ内ネットワークスタック(veth)やループバック処理におけるkTLSのオーバーヘッドに起因すると考えられます。

### 4.3. musl版のファイル配信エラー
`Landlock` の `landlock_read_paths` が修正（`/etc/veil`, `/lib`, `/usr`等の追加）された状態でも、`veil_musl` の HTTP/2 応答は正常に配信されず (スループットが数KB/sレベル)、高速にエラー応答を返し続ける状態でした。
このことから、musl環境におけるエラーの原因はLandlockの設定不足ではなく、`io_uring` がmusl版でファイルを読み出す際に発生している非互換性、または別のサンドボックス制限（seccomp等）にあることが強く示唆されます。

### 4.4. Nginx (ベースライン) との比較
Nginx (Alpine) は HTTP/2 (90.83 MB/s) および HTTP/1.1 (110.19 MB/s) の両方で極めて安定した結果を示し、エラーレスポンスもゼロでした。
一方で、Veil (glibc, no_ktls) の HTTP/2 スループットは **104.88 MB/s** を記録しており、**NginxのHTTP/2を明確に上回るポテンシャル**を持っています。非同期I/Oとルーティング周りの不具合が解決されれば、非常に強力なプロキシサーバーとなることが期待できます。

## 5. 推奨アクション (Next Steps)

1. **HTTP/1.1 パーサーとルーティングの調査:** 
   HTTP/1.1 経由でのみ 404 が発生する事象を解決するため、`src/` 配下の HTTP/1.1 要求解析（URIパスとHostヘッダーの処理）にフォーカスしたデバッグが必要です。
2. **musl 版固有の I/O エラーの調査:**
   `Landlock` の権限付与では解決しなかったため、`io_uring` 実行時や `seccomp` により musl libc で利用されるシステムコール（例: `stat`, `openat` など）がブロック・失敗していないか、トレースを用いた調査が推奨されます。
3. **コンテナデプロイでの kTLS 無効化:**
   Docker 等での運用時は、当面 `ktls_enabled = false` をデフォルトとすることでパフォーマンスの最大化が図れます。
