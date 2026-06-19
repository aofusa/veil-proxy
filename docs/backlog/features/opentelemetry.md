# バックログ: OpenTelemetry 対応

## 目的

Prometheus に加え、**トレース・ログ・メトリクス**を OTel エコシステムへ送出し、Grafana Tempo / Jaeger / OTLP コレクタ等と連携する。

## スコープ案

1. **信号の優先順位**
   - Phase 1: **OTLP/gRPC メトリクス**（Prometheus と二重出力の可否）。
   - Phase 2: **分散トレース**（リクエスト ID、upstream 呼び出しスパン）。
   - Phase 3: **ログ相関**（trace_id 注入、ftlog との橋渡し）。

2. **実装方針**
   - `opentelemetry` / `opentelemetry-otlp` クレートの採用、または軽量な OTLP 直書きの比較（依存サイズと async ランタイムとの整合）。
   - **tokio 非依存**を維持するため、エクスポートは専用スレッドまたは monoio 互換の非同期パスを検討（[AGENTS.md](../../../AGENTS.md) の設計哲学・設計制約）。

3. **設定**
   - OTLP エンドポイント、サービス名、サンプリング率、バッチサイズ。

## 受け入れ条件（案）

- サンプル compose（collector + Veil）でトレースが端到端表示される。
- メトリクス無効・トレースのみ等の **組み合わせ**が設定で可能。

## リスク

- 高負荷時のエクスポートバッファによるメモリ増。バックプレッシャーとドロップポリシーを明示。
