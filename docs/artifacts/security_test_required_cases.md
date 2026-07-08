# Veil セキュリティテスト 必要テストケース一覧

`Cargo.toml` の `full` features および `default` features を対象に、
`tools/container_security` で網羅すべきセキュリティテストケースを列挙する。
正の機能定義は [Cargo.toml](../../Cargo.toml)、設定キーは [examples/config.toml](../../examples/config.toml)。

## 1. ビルド feature とテスト観点

| Feature | セキュリティ観点 | 必須テストカテゴリ |
|---------|------------------|-------------------|
| `ktls` (default) | TLS オフロード・暗号設定 | testssl, TLS ハンドシェイク |
| `http2` (default) | H2 プロトコル攻撃面 | h2spec, HPACK/frame libFuzzer |
| `mimalloc` (default) | メモリ安全性 | chaos_load, ASAN/TSAN (opt-in) |
| `http3` | QUIC/H3 攻撃面 | HTTP/3 プローブ, UDP 到達性 |
| `grpc` / `grpc-web` / `grpc-full` | gRPC フレーミング・メタデータ | gRPC 不正プローブ (H2C) |
| `wasm` | Proxy-Wasm ABI・リソース枯渇 | WASM コンプライアンス, libFuzzer wasm_* |
| `compression` | 圧縮爆弾 (Zip/Gzip Bomb) | compression_cache_probe |
| `cache` | キャッシュポイズニング・デセプション | compression_cache_probe |
| `admin` | 認証・認可バイパス | admin_security_probe |
| `l4-proxy` | L4 接続枯渇・パススルー | l4_flood_probe |
| `websocket` | Upgrade 悪用・巨大フレーム | websocket_probe |
| `rate-limit` | 閾値遮断 (429) | rate_limit_probe |
| `buffering` | 低速クライアント占有 | slowloris (既存) + buffering 経路 |
| `metrics` | メトリクス DoS | metrics_probe |
| `opentelemetry` | OTLP 異常送信耐性 | otel_probe |
| `access-log` | ログ経路の安定性 | access_log_probe (ヘルス維持) |
| `jemalloc` / `system-allocator` | アロケータ差分 | perf/CI マトリクス (container_security は glibc full 既定) |

## 2. プロトコル・トランスポート

| ID | テストケース | 対象 feature | 期待結果 |
|----|-------------|--------------|----------|
| P-01 | HTTP/1.1 ブラックボックスファジング | default | クラッシュ・ハングなし |
| P-02 | HTTP/2 h2spec (TLS + H2C) | http2 | 準拠・脆弱フレーム拒否 |
| P-03 | HTTP/3 QUIC ハンドシェイク + GET | http3 | 200 または graceful エラー、プロセス生存 |
| P-04 | HTTP リクエストスマグリング (CL.TE/TE.CL) | default | 400 拒否 |
| P-05 | TLS 設定スキャン (testssl) | ktls | 脆弱スイート・ダウングレードなし |
| P-06 | gRPC 不正 Content-Type / 巨大メタデータ | grpc | 4xx/5xx、クラッシュなし |
| P-07 | gRPC H2C フレーミング異常 | grpc + http2 | GOAWAY/4xx、ハングなし |
| P-08 | WebSocket 不正 Upgrade / 巨大 Key | websocket | 4xx、クラッシュなし |
| P-09 | WebSocket 仕様外コントロールフレーム試行 | websocket | 接続拒否または切断、生存 |
| P-10 | L4 TCP 大量接続 (4443) | l4-proxy | HTTP/443 経路が生存 |
| P-11 | L4 TLS パススルー接続試行 | l4-proxy | 接続確立または graceful drop |

## 3. 拡張機能・設定

| ID | テストケース | 対象 feature | 期待結果 |
|----|-------------|--------------|----------|
| E-01 | Admin: トークンなし `/__admin/config` | admin | 401 |
| E-02 | Admin: 不正 Bearer トークン | admin | 401 |
| E-03 | Admin: 正しい Bearer で GET /config | admin | 200 (secret マスク) |
| E-04 | Admin: POST /reload 未認証 | admin | 401 |
| E-05 | Admin: 未知エンドポイント `/__admin/evil` | admin | 404 または未処理 |
| E-06 | Admin: メソッド改ざん PUT /config | admin | 404/405 |
| E-07 | 圧縮爆弾 (高圧縮率 gzip POST) | compression | 413/切断、OOM なし |
| E-08 | Accept-Encoding 応答圧縮 (gzip/br/zstd) | compression | 正常圧縮、クラッシュなし |
| E-09 | キャッシュデセプション (X-Forwarded-Host) | cache | ポイズニングキー非混入 |
| E-10 | キャッシュヒット整合性 | cache | 同一キーで安定応答 |
| E-11 | レートリミット超過 (429) | rate-limit | 429 Too Many Requests |
| E-12 | レートリミット下は正常応答 | rate-limit | 200 |
| E-13 | Prometheus `/__metrics` 高頻度アクセス | metrics | 応答継続、クラッシュなし |
| E-14 | OTLP 送信先到達不能でも生存 | opentelemetry | プロセス生存 |
| E-15 | 構造化アクセスログ有効時の負荷 | access-log | ヘルス維持 |

## 4. Proxy-Wasm (wasm feature)

| ID | テストケース | 期待結果 |
|----|-------------|----------|
| W-01 | header_filter 適用: `X-Veil-Processed: true` 付与 | Proxy-Wasm v0.2.1 準拠動作 |
| W-02 | フィルタ経路で 200 応答 | モジュール実行完走 |
| W-03 | 並行リクエスト (10+) でフィルタ適用 | 競合なし (B-05 回帰) |
| W-04 | 不正 WASM モジュールパス | 起動失敗またはルート 5xx、クラッシュなし |
| W-05 | libFuzzer `wasm_abi` / `wasm_host_abi` | クラッシュなし (RUN_WASM_FUZZ=1) |
| W-06 | fuel/timeout 超過モジュール | 切断または 5xx、ワーカー占有なし |

## 5. 静的解析・依存関係

| ID | テストケース | 期待結果 |
|----|-------------|----------|
| S-01 | semgrep SAST | 既知ルール違反なし (警告はトリアージ) |
| S-02 | clippy disallowed-methods (F-88) | ホットパス同期 I/O 検出、警告ゼロ |
| S-03 | cargo-audit / cargo-deny | advisory トリアージ |
| S-04 | gitleaks | シークレット誤コミットなし |
| S-05 | Trivy イメージスキャン | HIGH/CRITICAL トリアージ |

## 6. カオス・ファジング (既存 + full 経路)

| ID | テストケース | 対象 |
|----|-------------|------|
| C-01 | chaos_load + SIGHUP | 全経路生存 |
| C-02 | Toxiproxy 遅延/reset | /api/ 経路 (buffering) |
| C-03 | circuit_breaker | upstream 障害 |
| C-04 | slowloris | backpressure |
| C-05 | libFuzzer HPACK/frame/header/config | クラッシュなし |
| C-06 | libFuzzer io_uring_executor | 異常 CQE 耐性 |
| C-07 | bad_backend (opt-in) | プロトコル違反耐性 |
| C-08 | resource_exhaustion (opt-in) | OOM/panic なし |
| C-09 | syscall_chaos (opt-in) | inject 耐性 |
| C-10 | e2e_sanitizer (opt-in) | ASAN/TSAN |

## 7. full features 組み合わせマトリクス

`full` ビルドでは以下が **同時有効** となる。container_security の
`fixtures/veil-config.toml` で全経路を有効化し、上記テストを一括実行する。

```
ktls + http2 + http3 + grpc-full + wasm + compression + cache +
metrics + websocket + rate-limit + buffering + admin + access-log +
l4-proxy + opentelemetry + mimalloc
```

アロケータ排他 (`jemalloc` / `system-allocator`) は CI マトリクス
(glibc/musl nightly) で別途検証。container_security 既定は `veil:glibc` + `full`。