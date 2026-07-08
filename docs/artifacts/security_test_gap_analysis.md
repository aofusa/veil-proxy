# Veil セキュリティテスト ギャップ分析（F-90 実装後）

[security_test_required_cases.md](security_test_required_cases.md) と
[security_test_implemented_cases.md](security_test_implemented_cases.md) の差分。

## 解消済み（F-90）

| ギャップ（coverage_report） | 対応 |
|---------------------------|------|
| HTTP/3 専用プローブなし | `http3_probe.sh` |
| admin 認可バイパステストなし | `admin_security_probe.sh` |
| 圧縮爆弾・キャッシュ攻撃なし | `compression_cache_probe.sh` |
| L4 フラッドテストなし | `l4_flood_probe.sh` |
| WebSocket 検証なし | `websocket_probe.sh` |
| gRPC 専用プローブなし | `grpc_probe.sh` |
| rate-limit 429 検証不足 | `rate_limit_probe.sh` |
| WASM DAST/コンプライアンス不足 | `wasm_security_probe.sh` |
| metrics DoS 不足 | `metrics_probe.sh` |
| OTLP 異常系不足 | `otel_probe.sh` |
| F-88 clippy lint 未完了 | `clippy.toml` + CI |

## 残ギャップ（意図的・別チケット）

| ID | 内容 | 理由 / チケット |
|----|------|----------------|
| P-03b | h3spec 相当の厳密 H3 準拠 | h3spec ツール未整備。HTTP/3 は curl/到達性で代替 |
| P-09b | WebSocket 巨大ペイロード双方向 | 専用 WS バックエンド要。正常系は E2E |
| P-11 | L4 TLS パススルー完全性 | 証明書付きクライアント要。生存検証のみ |
| W-04〜W-06 | 不正モジュール・fuel 枯渇 | libFuzzer/E2E が担当。container は header_filter 正常系 |
| S-02 | clippy 全ターゲット `--all-targets` | CI は `--tests` のみ。bins 追加は別 PR |
| C-07〜C-10 | 拡張カオス | 既定 SKIP（特権・時間）。F-67〜F-86 で実装済み |
| F-83 | Nuclei / Landlock 発火 | 未着手（F-83） |
| grpc-web / http3+gRPC | grpc-full 全組合せ | E2E + 将来 h3+gRPC プローブ |

## アロケータマトリクス

`jemalloc` / `system-allocator` は container_security 既定外。
nightly glibc/musl マトリクス（F-57）でイメージ差分を検証する。