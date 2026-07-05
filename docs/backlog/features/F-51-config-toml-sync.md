# F-51: config.toml を src/config.rs と完全同期（網羅・重複・抜け漏れ排除）

- **優先度**: P1
- **対応状況**: 完了（2026-07-02）

## 完了メモ

- 全 `src/**/*.rs` の Deserialize 構造体フィールドと examples/config.toml のキー（コメント例示含む）を機械的に突き合わせて同期。
- **追記**: `[route.security]` の未記載 11 キー（chunked 上限・クライアントタイムアウト・接続プール・remove_response_headers・WebSocket ポーリング 4 種）、WASM `[wasm.modules.capabilities]` の未記載 7 キー（metrics / shared_data / send_local_response / max_http_calls / properties 系）、L4 `idle_timeout_secs`、（F-50 で `cipher_suites`）。
- **削除（stale）**: `[grpc]` セクション全体（`enabled`/`timeout_ms`/`max_message_size`/`encodings`/`compression_level`/`min_compression_size`/`[grpc.grpc_web]`/`[route.grpc_web_cors]`）— ルート `Config` 構造体に対応フィールドが無く、パーサに無視される存在しない設定のドキュメントだった。gRPC は feature フラグ + `use_h2c` ルートで構成する旨の正確な記述に置換。
- **dead code 削除**: `RetryPolicy` 構造体（F-06 で定義のみされ、リトライ機構本体が未実装でどこからも未参照）を削除。resilience.rs のコメントも修正。
- **回帰防止**: 同梱 examples/config.toml がパース・バリデーション可能であることを保証する単体テスト `shipped_config_toml_parses_and_validates`（`veil -t` 相当、プレースホルダーパスのみ差し替え）を追加。
- README.md / docs/readme/README.ja.md の `[tls]` 設定表に `cipher_suites` / `auto_reload` / `reload_interval_secs` を追記。
- **出典**: ユーザー要求

## 機能説明・現状

`src/config.rs`（約 6,600 行）の設定構造体と、リポジトリ同梱の `examples/config.toml`（約 1,900 行）の間に乖離がある可能性がある。運用者は **examples/config.toml と README.md だけを見れば全設定を把握できる**状態であるべき。

## 改修内容

1. `src/config.rs` の全 serde 設定項目（フィールド名・型・default・エイリアス）を機械的に洗い出す。
2. `examples/config.toml` に:
   - **抜け漏れ**（config.rs にあるが toml に記載なし）を追記（既定値 + 説明コメント付き、必要ならコメントアウト例示）。
   - **重複・廃止項目**（toml にあるが config.rs に存在しない/重複記載）を削除・統合。
3. README.md / docs/readme/README.ja.md の設定説明との整合を取る。

## 受け入れ条件

- config.rs の全設定キーが examples/config.toml に記載（有効値またはコメント例示）されている。
- examples/config.toml に存在しないキー・重複キーが無い。
- 同梱 examples/config.toml が実際にパース・起動可能（統合テストで検証）。

## 依存・リスク

- F-50（cipher_suites）追加後に実施し、新設定も反映する。
