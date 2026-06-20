# バックログ: 高度なロードバランシング (Weighted, Consistent Hash等)

## 目的

既存の基本的なロードバランシングアルゴリズム（Round Robin, Least Connections, IP Hash）を拡張し、複雑なマイクロサービス環境や段階的ロールアウトに対応する。前回のEnvoyとの比較で劣っていた「高度なトラフィックシェーピング」を補うための機能。

## スコープ案

1. **追加アルゴリズムの実装**
   - Weighted (重み付け) 分散: A/B テストやカナリアリリース用。
   - Consistent Hash (Ring Hash, Maglev 等): キャッシュヒット率の向上用。
   - Zone-aware routing: ゾーンを跨ぐ通信の抑制。
2. **動的構成 (dynamic config) との統合**
   - F-04 と連動し、再起動なしに upstream の重みを変更できるようにする。

## 受け入れ条件（案）

- Weighted アルゴリズムを利用して、指定した比率通りにトラフィックが分散されることをテストで確認する。
- Consistent Hash によって、同一キー（ヘッダやパス）に対して同じ upstream が選定されることを確認する。

## 依存・リスク

- F-04 (dynamic config) および F-06 (resilience) と密接に関わるため、併せて設計の整合性を取る。
- 状態を持つ LB アルゴリズムの実装において、パフォーマンス影響がないよう最適化すること。
