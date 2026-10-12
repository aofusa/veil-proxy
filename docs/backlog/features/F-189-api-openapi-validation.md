# F-189: OpenAPI 自動生成と自動バリデーション（API ライブラリ）

**状態: 進行中（feat/f188-api-library-vds-k8s）**

## 目的

マクロで定義したルートから FastAPI と同様に OpenAPI 3.1 を自動生成し、Swagger UI を公開する（公開可否・パスは設定）。
`#[derive(veil::Schema)]` の `#[validate(range(min, max))]`・`length`・`email` 等を JSON Schema（`minimum`/`maximum`/
`minLength`/`maxLength`/`format`）に反映し、同じ定義で実行時の自動バリデーション（422）を行う。

## 設計

- feature `openapi`（`handler` + `macro`）。生成は起動時（ルート確定後）に 1 回だけ行い `Bytes` で保持（ホットパス外）。
- `[api] openapi_enabled` / `openapi_path`（Swagger UI、既定 `/docs`）/ `openapi_json_path`（既定 `/openapi.json`）。
- C / Python から登録されたルートもメタデータ（JSON Schema）を受け取ってマージする。
- `validator` クレートと同じ `#[validate(...)]` 記法を受け付ける（validator の derive と併用可能）。

## 受け入れ条件

- 単体テスト: スキーマ生成（min/max・必須・Option・ネスト・列挙）、バリデーションのエラー形式。
- E2E: `/openapi.json` の内容と `/docs` の HTML、バリデーション失敗が 422。
