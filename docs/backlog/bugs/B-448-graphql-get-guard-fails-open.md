# B-448: GraphQL の GET の mutation の検査が、空・解析できない文書を mutation でないとして通し、persisted query で保存した mutation を GET で実行できた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「GET mutation guard fails open when the query is empty/unparseable, so a persisted-query GET runs a cached mutation
(CSRF with SameSite=Lax cookies)」から確認。

## 事象

- `veil_kit::graphql`（Rust の kit）の `is_mutation` は、文書を解析できなければ `false`（mutation でない）を返して
  実行へ回していた。`query` の無い GET は空の文書で、解析できない。
- async-graphql の `ApolloPersistedQueries` の拡張は、空の文書と `extensions.persistedQuery.sha256Hash` を受けると
  保存した文書へ差し替えて実行する。保存した文書が mutation でも、別のサイトのリンク（トップレベルの GET には
  `SameSite=Lax` の Cookie が付く）から利用者として実行できた（B-196 の迂回）。
- 同じ検査を写した `examples/api/rust-graphql` と、C の helpers（`veil_kit_graphql`。操作の無い文書を mutation で
  ないとして実行のコールバックへ回す）も同じだった。

## 対応

- GET では、空の文書・解析できない文書・操作を選べない文書を実行しない（Rust の kit は GraphQL のエラーの 400、
  例と C の helpers は 400）。persisted query のハッシュは POST で送る（Apollo Client の既定）。
- 利用者ガイド（`library-kit.md` の英日）に書いた。

## テスト

- `veil_kit::graphql::tests::get_runs_queries_only`（persisted query の参照だけの GET・解析できない GET は 400）。
- `veil-capi-helpers` の `graphql::tests::operation_kinds`（操作を選べない文書）。
- `tests/api/library_e2e.sh` の「GraphQL GET without a document is 400」（例・kit・C の kit）。
