# B-422: ルートの条件の `source_ip` が読めない項目を黙って捨て、設定の全部の項目が読めないと IP を絞ったルートがどのクライアントにも一致した

**状態: 修正済み（fix/security-scan-fable-max、2026-11-17）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Route `conditions.source_ip` is never validated; when no entry parses, the router drops the restriction」・
「A route's source_ip restriction fails open when none of its entries parse」（同じ指摘が 2 件）から確認。

## 事象

- `[route.conditions] source_ip`（設定ファイル・VDS のルート）は設定の検査（`validate_route_config`）が見ておらず、
  `routing::CidrMatcher::add_cidr` は読めない項目（`192.168.1.*`・ホスト名・前後の空白・長すぎる接頭辞）を黙って
  （長すぎる接頭辞は警告だけで）捨てていた。
- 設定の全部の `source_ip` が読めないと `has_conditions()` が偽になり、`get_candidates` は全ルートを IP の候補にした。
  その後の照合（`matches_remaining_conditions`）はヘッダ・メソッド・クエリしか見ないので、IP を絞ったつもりのルート
  （管理用の上流等）がどのクライアントにも一致した（`allowed_ips` の同じ倒れ方は B-322 で直している）。

## 対応

- `validate_route_config` で `conditions.source_ip` の各項目を `[route.security]` の IP の一覧と同じく検査し、
  読めない項目は設定の誤りにする（起動・リロード・VDS の適用で断る）。
- ルーターは `source_ip` のあるルートが 1 つでもあれば IP の条件を見る（読める項目の無いルート・空の一覧のルートは
  どの IP にも一致しない）。項目の前後の空白は読む。
- 利用者ガイド（`routing.md` の英日）と `examples/config.toml` に書いた。

## テスト

- `config::validate::b422_tests::route_source_ip_entries_must_parse`・
  `routing::tests::unparsable_source_ip_fails_closed`。
