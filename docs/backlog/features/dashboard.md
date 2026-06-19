# バックログ: ダッシュボード機能

## 目的

運用者が **ブラウザから Veil の状態**（ヘルス、upstream、設定サマリ、メトリクス）を把握できるようにする。

## スコープ案（段階的）

1. **MVP**
   - 既存 Prometheus メトリクスを **埋め込み簡易 UI** または **Grafana なしの静的チャート**（オプション）。
   - 認証: ローカルホスト限定、mTLS、または Basic 認証。

2. **拡張**
   - SIGHUP リロード履歴、現在のルート一覧、upstream のブレーカー状態（実装後は [resilience-outlier-detection.md](resilience-outlier-detection.md) と連携）。
   - 読み取り専用 API（JSON）＋ SPA の分離も可。

3. **セキュリティ**
   - 管理面は **デフォルト無効**、有効時は強い認証必須を README に明記。

## 技術選択（未決）

- Rust 内蔵（axum/warp 等） vs 静的ファイル＋別プロセス。Veil の **tokio less** 方針との整合を検討（管理用のみ別スレッドで tokio 等を許容する案もあり）。

## 受け入れ条件（案）

- 本番デフォルトオフ、有効化手順と threat model の概要が文書化されている。

## 依存

- [prometheus-feature-flags.md](prometheus-feature-flags.md)、将来 [opentelemetry.md](opentelemetry.md)。
