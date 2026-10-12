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

---

## 決定した設計（2026-10-12、feat/f188-api-library-vds-k8s）

**状態: 完了（feat/f188-api-library-vds-k8s）**

- **A. 内蔵**を採用（feature `acme`、`full*` に含める、`[acme]` セクション）。HTTP-01 チャレンジ。
- ACME クライアントは専用スレッドで動かし（データプレーン外）、署名は ES256（aws-lc-rs、OpenBSD/NetBSD は ring）。
- HTTP-01 の応答は `challenge_listen` の専用平文リスナー（専用スレッド）で返す。メインのデータプレーンは変更しない。
- 発行した証明書は `[tls].cert_path` / `key_path` に原子的に書き込み、既存の証明書ホットリロード（F-03）で無停止に差し替える。
  起動時に証明書が無ければ自己署名の仮証明書を置いて起動する。期限の `renew_before_days` 日前に更新。
- E2E は Pebble（Let's Encrypt のテスト用 ACME サーバ）。

## 結果（2026-10-12、feat/f188-api-library-vds-k8s）

- `src/acme/`（JWS ES256・JWK サムプリント・RFC 8555 のフロー・HTTP-01 応答リスナー・notAfter の解析）、
  起動時の仮自己署名証明書、原子的な書き込みと証明書ホットリロード、期限前の更新。`docs/guide/acme.md`（英日）。
- 単体テスト、Pebble での E2E（`tests/acme/pebble_e2e.sh`: 仮証明書 → 発行 → 差し替え → 配信 → 鍵の権限 600）。
