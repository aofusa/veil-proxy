# F-50: [tls] cipher_suites 設定（nginx 風の取捨選択・優先度指定）

- **優先度**: P1
- **対応状況**: 未着手
- **出典**: ユーザー要求

## 機能説明・現状

現在 TLS の暗号スイートは `ktls_compatible_cipher_suites()`（kTLS 互換の固定リスト）またはビルトインの既定にハードコードされており、運用者が nginx の `ssl_ciphers` のように**利用スイートの取捨選択・優先順位付け**を行えない。

## 改修内容

1. `config.toml` の `[tls]` セクションに `cipher_suites = ["TLS13_AES_256_GCM_SHA384", ...]` 形式の設定を追加する。
   - 配列の**記載順 = 優先度順**（rustls のスイート順序はサーバ選好に使われる）。
   - 未指定時は従来挙動（既定リスト）。
2. rustls の `ServerConfig` 構築時（クライアント側 TLS / バックエンド側 TLS 双方）に指定スイートのみを、指定順で適用する。
3. kTLS 有効時: kTLS 非互換スイートが指定された場合の挙動（警告して kTLS を無効化 or 起動エラー）を明確化する。
4. 不正なスイート名は起動時 / 設定検証時にエラーとして拒否する。

## 改修案

- rustls `ALL_CIPHER_SUITES` からの名前解決テーブルを実装（TLS1.3 / TLS1.2 双方）。
- `config.rs` に serde デシリアライズ + validate を追加。
- E2E: openssl / rustls クライアントでネゴシエートされたスイートを検証。

## 受け入れ条件

- `cipher_suites` の指定順どおりのスイートがネゴシエートされる E2E が通過。
- 不正名の設定はバリデーションエラーになる。
- kTLS との組み合わせ挙動がドキュメント化されている（README / config.toml コメント）。

## 依存・リスク

- kTLS は AES-GCM 系のみ対応のため、CHACHA20 等を優先した場合 kTLS スプライス経路が使えない。挙動の明示が必須。
