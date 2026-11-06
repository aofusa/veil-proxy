# B-322: 認可・経路・送信の検査漏れ（JWT の期限・IP リスト・リダイレクト・gRPC のパス・平文の送信・HPACK）

**状態: 修正済み（v0.11.0）**

## 事象

v0.11.0 前のセキュリティスキャン（Claude Security の F33・F35・F37・F39・F42・F45・F46・F47・F56）で判明。

- **F33**: JWT の `exp`・`nbf` を `as_u64` で読み、小数（RFC 7519 が許す）・文字列・負の値を「無い」と同じに扱い、
  既定（`require_expiration = false`）で期限の無いトークンとして通していた。
- **F46**: `[route.security] allowed_ips`・`denied_ips` の読めない要素を黙って捨て、`allowed_ips` が全部読めないと
  空＝全許可になっていた（設定の検査もしていなかった）。
- **F56**: `path = "/"` のルートでは `$path` が `/` で始まらず、`https://new.example.com$path` が `/@evil.com` で
  `https://new.example.com@evil.com`（別のホスト）になった（オープンリダイレクト）。
- **F45**: クライアントの `content-type: application/grpc` だけで上流のパスの書き換え（接頭辞の除去・URL のパス）を
  外し、設定の対応付けの外のパスを上流へ要求できた。
- **F47**: ProxyUpstream の `use_h2c` をグループ全体に付け、HTTP/2・HTTP/3 の経路が `https://` のサーバへ
  平文の h2c で要求を送っていた。
- **F42**: キャッシュの裏の再検証が `use_tls` を見ずに平文で、利用者の Cookie・Authorization を付けて送っていた。
- **F37**: OTLP のエクスポータは `https://` を外して平文で送っていた。
- **F39**: 上流の HTTP/2 の共有の接続で、Authorization・Cookie を HPACK の動的テーブルに入れていた（他の利用者の値を
  推測できる）。
- **F35**: HTTP/3 のバッファ経路は上流の Content-Length で本文のバッファを確保していた（上限の検査は読みながら）。

## 修正

- JWT: NumericDate は小数も読み、負でない数でない `exp`・`nbf` は拒否する。
- IP リスト: route の `allowed_ips`・`denied_ips`・`trusted_proxies` と `[security] blocked_ips` の全要素を設定の
  読み込み（起動・リロード）で検査する。`IpFilter` は前後の空白を許し、許可リストが 1 つも読めなければ全部拒否する。
- リダイレクト: 3 プロトコル共通の `http_utils::build_redirect_url` にし、`$path` は必ず `/` で始める。
- gRPC のフルパスは上流の URL にパスが無いときだけ保つ。
- `UpstreamGroup::with_h2c` はサーバごとに付け（TLS には付けない）、グループの印は全サーバが平文のときだけ。
- 裏の再検証は TLS の上流と資格情報の付いた要求では行わない。
- OTLP の `https://` は設定の読み込みで拒否する。
- 上流の HTTP/2 で authorization・proxy-authorization・cookie・set-cookie を never-indexed で送る。
- HTTP/3 のバッファ経路は上限（64 MiB）を超える Content-Length を確保の前に断る。

文書: docs/guide/routing.md・observability.md（英日）。単体試験 `jwt_verify::tests::numeric_dates_fractional_and_invalid`・
`config::net_policy::tests::unparsable_allow_list_denies_all`・`http_utils::tests::redirect_path_keeps_leading_slash`・
`proxy::tests::grpc_full_path_only_without_target_prefix`・`config::upstream_group::h2c_tests`・`otel::tests::parse_endpoint_variants`。

## 関連

- docs/artifacts/security/v0110-claude-security/（F33・F35・F37・F39・F42・F45・F46・F47・F56）
