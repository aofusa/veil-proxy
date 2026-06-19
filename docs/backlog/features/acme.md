# バックログ: ACME 統合（Let's Encrypt 等）

## 目的

- 手動で証明書を配置する運用を減らし、**自動発行・更新**を Veil または同梱ツールで支援する。

## スコープ案

1. **統合レベルの選択**
   - **A. 内蔵**: Veil プロセス内で ACME クライアント（HTTP-01 / TLS-ALPN-01 等）を実行。
   - **B. 外部**: certbot / lego 等と連携し、Veil は **証明書ファイルの更新検知とホットリロード**のみ担当（[tls-cert-zero-downtime.md](tls-cert-zero-downtime.md) とセット）。

2. **チャレンジとプロキシの関係**
   - HTTP-01: `.well-known/acme-challenge` をバックエンドに透過するか、Veil が直接応答するか。
   - TLS-ALPN-01: 443 の ALPN ハンドシェイクに一時的な応答が必要。

3. **秘密情報**
   - アカウント鍵、証明書ストレージパス、権限（非 root 実行時）を設定で明示。

## 受け入れ条件（案）

- ステージング ACME での E2E（発行 → 配置 → Veil が新証明書で応答）。
- README に「推奨運用（内蔵 vs 外部）」が書かれている。

## 依存

- 0 ダウンタイム TLS ローテーションとの設計すり合わせ。
- レート制限（Let's Encrypt）と再試行ポリシー（[resilience-outlier-detection.md](resilience-outlier-detection.md) と思想を共有）。

## リスク

- チャレンジ用の一時的なリスナーと既存リスナーのポート競合。
