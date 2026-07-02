# バックログ: ファジング・カオスエンジニアリング・セキュリティスキャン

## 目的

プロトコルパーサ・バッファ処理・TLS 境界の **堅牢性** と、本番に近い障害下での **生存性** を継続的に検証する。

## スコープ案

### 1. プロトコル相互運用・準拠

| ツール | 狙い |
|--------|------|
| **h2spec** | HTTP/2 実装の準拠・エッジケース |
| **QUIC interop**（quiche エコシステム等） | HTTP/3 / QUIC の相互運用 |

- CI で毎回は重いため、**nightly または週次**ジョブ案。

### 2. ファジング（LibAFL 等）

- 対象候補: HTTP/1 パース、HPACK、カスタムバッファ操作、設定パーサ。
- **libFuzzer / cargo-fuzz** から段階的に LibAFL 等へ拡張する道筋を記載。
- クラッシュ再現用の **最小シードコーパス** をリポジトリまたは別アーティファクトストアに。

### 3. カオスエンジニアリング（Chaos Mesh 等）

- Kubernetes 上で Veil を動かす前提が後続（[post-stability-containerization.md](post-stability-containerization.md)）になる場合が多い。
- シナリオ例: upstream 遅延・断続的ドロップ、DNS 失敗、ノード CPU 圧迫。
- 非 K8s 環境では **tc / iptables** スクリプトによる簡易代替も文書化。

### 4. セキュリティスキャン

- **cargo audit** / **deny**（依存脆弱性）。
- **SAST**（任意の商用 / OSS）。
- **コンテナスキャン**（イメージ化後）。

## 受け入れ条件（案）

- 少なくとも 1 系の **自動ファズ** または **h2spec** を CI に組み込むか、手順が `docs/` に固定されている。
- 重大度の脆弱性に対する対応フロー（SLA なしでもチェックリスト）を README または SECURITY に。

## リスク

- CI 時間・フレーク。ファジングは並列ワーカーとタイムボックスが必須。

---

## 将来拡張（F-07 残件）

[post-stability-containerization.md](post-stability-containerization.md)（F-14）で `docker/` 基盤が整備済み。以下はコンテナ運用（docker-compose / Kubernetes）を見据えた追加機能として backlog に残す。

### 1. 設定のランタイム上書き

| 手段 | 想定用途 |
|------|----------|
| **環境変数** | `VEIL_SERVER_LISTEN` 等、機密を含まないキーのオーバーライド。既存 TOML（serde）との優先順位を定義する。 |
| **CLI 引数** | `-c` 以外に `--set key=value` 形式で起動時のみ上書き。検証（`-t`）経路にも反映する。 |

- 受け入れ条件案: README / docker README に env・CLI の対応表。E2E（コンテナテスト）で代表キーが上書きされること。

### 2. 証明書・秘密鍵の安全な受け渡し

| 手段 | 想定用途 |
|------|----------|
| **ファイルマウント（read-only）** | 現行 `docker/assets/ssl/` + `-v ...:ro`（開発・単一ノード）。 |
| **Docker secrets / K8s Secret** | ボリュームマウント `/run/secrets/tls-cert` 等。パスは env で `config.toml` の `cert_path` / `key_path` と整合。 |
| **環境変数（PEM 本文）** | `VEIL_TLS_CERT_PEM` / `VEIL_TLS_KEY_PEM`（またはファイルパス）。起動時に tmpfs へ書き込まずメモリ上のみで保持する設計を検討。 |
| **SIGHUP リロード** | シークレット差し替え後のゼロダウンタイム更新（F-03 連携）。 |

- 受け入れ条件案: compose / K8s マニフェストのサンプル（参考実装）と、コンテナセキュリティテストで TLS ハンドシェイクが成立すること。

---

## 実施記録

（テスト実施後に追記）
