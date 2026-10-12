# F-191: GraphQL 対応（async-graphql、API ライブラリ）

**状態: 完了（feat/f188-api-library-vds-k8s）**

## 目的

async-graphql のスキーマを veil のハンドラとして公開する（GraphQL over HTTP: POST JSON / GET クエリ文字列、GraphiQL）。

## 設計

- feature `graphql`。async-graphql は実行時ランタイム非依存のため veil のワーカー上で直接 await する。
- `App::graphql(path, schema)`。GET かつ `Accept: text/html` なら GraphiQL を返す。サブスクリプションは非対応。

## 受け入れ条件

- E2E: クエリ・ミューテーション・変数・エラー応答、GraphiQL の HTML。

## 結果（2026-10-12、feat/f188-api-library-vds-k8s）

- `App::graphql(path, schema)`（POST JSON・GET クエリ文字列・バッチ・GraphiQL）。E2E でクエリ・GET・GraphiQL を確認。
