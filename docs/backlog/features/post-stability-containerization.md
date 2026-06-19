# フェーズ 2: コンテナ化対応（機能安定後）

## 目的

Docker / OCI 環境で **再現可能なデプロイ**を行い、Kubernetes 等との統合を容易にする。

## スコープ案

1. **Dockerfile**
   - マルチステージビルド、`--features` を build-arg で切替。
   - 非 root ユーザー、読み取り専用ルートファイルシステム想定。

2. **設定の環境変数化**
   - 機密は env または secrets マウント。`config.toml` の一部を env でオーバーライドするルールを定義（既存 serde との整合）。

3. **musl ターゲット**
   - 静的リンクイメージのサイズと互換性（aws-lc-rs、io_uring の可否）を検証。glibc イメージとの性能差を文書化。

## 受け入れ条件（案）

- `docker build` で代表 feature セットが通る。
- README に compose サンプル（オプション）。

## 前提

- 本番相当の E2E・セキュリティ方針が一通り固まってから着手（イメージ公開の責任）。

## 関連

- [fuzzing-chaos-security.md](fuzzing-chaos-security.md) の Chaos Mesh はコンテナ前提になりやすい。
