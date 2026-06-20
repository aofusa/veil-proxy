# バックログ: 構造化アクセスログと管理Admin API

## 目的

運用時のトラブルシューティングに必要な詳細なアクセスログ（JSON 等）と、運用者がリアルタイムに状態確認や操作を行える軽量な Admin API を提供する。Prometheus 以外の詳細な observability と動的な運用操作を可能にし、Envoy との比較での運用性の弱点を埋める。

## スコープ案

1. **構造化アクセスログ**
   - nginx のような柔軟なフォーマット定義。
   - JSON 形式での出力（Elasticsearch/Datadog 等でのパース容易化）。
2. **軽量 Admin API (`/admin`)**
   - Config のダンプ、詳細 Stats (Prometheus 以外のフォーマットや即時取得)。
   - Manual Reload のトリガー機能。
   - Cache Purge エンドポイントの統合。
3. **セキュリティ**
   - Admin API に対する IP 制限および認証機能の必須化。

## 受け入れ条件（案）

- JSON 形式でのアクセスログ出力が設定通りに機能すること。
- Admin API 経由で現在の設定や Stats が正常に取得できること。
- Admin API が適切に保護されていること。

## 依存・リスク

- Observability 強化の観点で F-10 (OpenTelemetry) と相補的な機能となる。
- Admin API 自体のルーティングが本来のデータプレーンと安全に分離されていること。
