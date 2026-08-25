# F-163: `[server].tls_only` — TLS リスナーの平文受理を既定で禁止する

## 背景

`[server].listen` は本来 TLS 終端用のリスナーだが、`h2c_enabled = true` のとき
`proxy.rs::handle_connection` が `detect_protocol_with_buffer`（MSG_PEEK）でプロトコルを
判別し、**平文 HTTP/1.1**（`accept_plain`）と **h2c** を同一ポートで受理していた。
TLS ポートに平文が通ることは、意図しない平文フォールバック（ダウングレード）を許す構成であり、
既定としては安全でない。

## 仕様

- `[server].tls_only`（bool、**既定 `true`**）を追加する。
- 適用範囲は **メインリスナー（`[server].listen`）のみ**。
  明示的に平文を指定する経路（`[server].h2c_listen`、`listen_http`（HTTPS リダイレクト用）、
  `[[l4]]`）は対象外＝設定どおり動く。
- `tls_only = true` のとき、メインリスナーでは **プロトコル検出そのものを行わない**。
  接続は常に TLS ハンドシェイクへ渡され、平文クライアントはハンドシェイク失敗で切断される。
  これは仕様上最も厳格であると同時に、接続ごとの MSG_PEEK 往復（最大 200ms 待ち）を
  丸ごと省くのでホットパス的にも有利（ゼロコスト）。
- `tls_only = false` のときは従来どおり（h2c / 平文 HTTP/1.1 をメインリスナーで受理）。
- `h2c_enabled = true` かつ `h2c_listen` が未指定または `listen` と同一の場合は
  **H2C 専用サーバ**（`entry.rs::is_h2c_only_server`）となり TLS リスナー自体が起動しないため、
  `tls_only` は影響しない（既存の h2c 構成・perf 構成・E2E は無変更で通る）。
  この組み合わせは起動時に info ログで明示する。

## 実装

- `config.rs`: `ServerConfig` に `#[serde(default = "default_tls_only")] pub tls_only: bool`（既定 `true`）。
  `LoadedConfig` / `RuntimeConfig` へ伝播（ホットリロード対象）。
- `proxy.rs`: `handle_connection`（`veil_ktls` 版・`simple_tls` 版の両方）のプロトコル検出ブロックの
  条件を `config.h2c_enabled` から `config.h2c_enabled && !config.tls_only` にする。
- ドキュメント: `examples/config.toml` / README / README.ja / AGENTS.md。

## テスト

- 単体: 既定値が `true` であること、`tls_only = false` の deser。
- E2E: `tls_only = true`（既定）で TLS ポートへ平文 HTTP/1.1 を投げると接続が失敗すること、
  `tls_only = false` では従来どおり平文が通ること。
