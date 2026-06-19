# バックログ: config.toml ジェネレータ Web UI

## 目的

複雑な `config.toml` を **フォームとバリデーション**から生成し、初心者・自動化の両方を支援する。

## スコープ案

1. **機能**
   - スキーマ駆動（Veil の `RuntimeConfig` 等から JSON Schema または専用定義を生成）。
   - 必須項目、デフォルト、ルート・upstream の追加ウィザード。
   - 「エクスポート: TOML」「インポート: 既存 TOML のパースと警告」。

2. **配置**
   - 静的 SPA + GitHub Pages / 自前ホスト、または Veil バイナリに **オプションで同梱**（feature フラグ）。
   - ダッシュボード（[dashboard.md](dashboard.md)）と UI コンポーネント共有の可否。

3. **プライバシー**
   - ブラウザ完結（サーバに設定を送らない）を第一候補にすると運用が簡単。

## 受け入れ条件（案）

- 代表サンプル（リバースプロキシ 1 バックエンド＋TLS）が UI から生成でき、実際に Veil が起動する。
- スキーマと Veil 本体のドリフト検知（CI でスキーマ生成をチェック）。

## 依存

- 設定構造の安定化。VDS（[vds-xds-dynamic-config.md](vds-xds-dynamic-config.md)）導入後は「中間表現 ↔ TOML」の両方を扱う必要ありうる。
