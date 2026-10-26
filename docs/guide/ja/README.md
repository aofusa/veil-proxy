# Veil ドキュメント

[← README](../../readme/README.ja.md) · [English](../README.md)

トップレベルの README から分割した利用者向けガイドです。各ページは元の節の本文をそのまま収録しています。

| トピック | ガイド |
|----------|--------|
| 機能一覧 | [docs/guide/ja/features.md](features.md) |
| プラットフォーム・ランタイムバックエンド | [docs/guide/ja/platforms.md](platforms.md) |
| ビルドとパッケージ | [docs/guide/ja/building.md](building.md) |
| 入れ方・設定ファイル・検証・リロード・停止 | [docs/guide/ja/running.md](running.md) |
| コンテナイメージ（すぐ試す・自分の設定とファイル） | [docs/guide/ja/container.md](container.md) |
| 設定リファレンス | [docs/guide/ja/configuration.md](configuration.md) |
| TLS・kTLS・証明書リロード | [docs/guide/ja/tls.md](tls.md) |
| ルーティング・リダイレクト・ヘッダー | [docs/guide/ja/routing.md](routing.md) |
| ロードバランシング・ヘルスチェック・レジリエンス | [docs/guide/ja/load-balancing.md](load-balancing.md) |
| L4 ストリームプロキシ | [docs/guide/ja/l4-proxy.md](l4-proxy.md) |
| HTTP/2・HTTP/3・WebSocket | [docs/guide/ja/protocols.md](protocols.md) |
| 圧縮・キャッシュ・バッファリング | [docs/guide/ja/compression-cache.md](compression-cache.md) |
| WASM 拡張 | [docs/guide/ja/wasm.md](wasm.md) |
| API サーバライブラリ（Rust・C・Python） | [docs/guide/ja/library.md](library.md) |
| Rust のヘルパークレート `veil-proxy-kit`（Cookie・CORS・JWT・セッション・DI・タスク） | [docs/guide/ja/library-kit.md](library-kit.md) |
| API ライブラリを本番で動かす（推奨の設定） | [docs/guide/ja/library-production.md](library-production.md) |
| VDS: 制御プレーンからの動的ルート配信 | [docs/guide/ja/vds.md](vds.md)（[スキーマ](../vds-schema.json)） |
| ACME: 証明書の自動発行・更新 | [docs/guide/ja/acme.md](acme.md) |
| Kubernetes（Helm chart、サイドカー、Istio / Linkerd） | [docs/guide/ja/kubernetes.md](kubernetes.md) |
| サービスメッシュのデータプレーン（Envoy・linkerd2-proxy の代わりに veil） | [docs/guide/ja/mesh.md](mesh.md) |
| メトリクス・Grafana のダッシュボード・トレーシング・ログ | [docs/guide/ja/observability.md](observability.md) |
| 管理 API・キャッシュ Purge API | [docs/guide/ja/admin-api.md](admin-api.md) |
| セキュリティとサンドボックス | [docs/guide/ja/security.md](security.md) |
| パフォーマンスチューニングとベンチマーク | [docs/guide/ja/performance.md](performance.md) |
| テスト（開発者向け） | [docs/guide/ja/testing.md](testing.md) |
| nginx からの移行 | [docs/guide/ja/migration-nginx.md](migration-nginx.md) |
| Envoy からの移行 | [docs/guide/ja/migration-envoy.md](migration-envoy.md) |
| 参考資料・ロゴ | [docs/guide/ja/references.md](references.md) |

その他の文書:

- [examples/config.toml](../../../examples/config.toml) — 全設定キーのコメント付きリファレンス
- [docs/perf/README.md](../../perf/README.md) — 最新のベンチマーク結果
- [docs/history/](../../history/) — 設計の経緯と教訓（開発者向け）
- [AGENTS.md](../../../AGENTS.md) — コントリビュータガイド
