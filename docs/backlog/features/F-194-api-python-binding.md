# F-194: Python バインディング（PyO3）— FastAPI 風デコレータ

**状態: 完了（feat/f188-api-library-vds-k8s）**

## 目的

Python から `@veil.route` / `@app.get(...)` のデコレータでルートとハンドラを登録し、veil のプロトコル処理・I/O 性能を
手軽に使えるようにする。Pydantic は任意（あれば本文の検証と OpenAPI に使う）。

## 設計

- 別クレート `crates/veil-python`（PyO3、abi3-py38、maturin、モジュール `veil._veil`）+ 純 Python の `veil` パッケージ。
- Python ハンドラは既定で offload（GIL を取って専用プールで実行）、`offload=False` でワーカー上で直接実行。
- 引数束縛は FastAPI と同じ規則（パスパラメータ・クエリ・本文・`req`）。`body_constraints` で pydantic 無しでも検証と OpenAPI 反映。

## 受け入れ条件

- wheel を venv にインストールしてサンプルを起動し、ルーティング・変換・検証（pydantic 有無）・OpenAPI を E2E で確認。

## 結果（2026-10-12、feat/f188-api-library-vds-k8s）

- `crates/veil-python`（PyO3 abi3、maturin、`veil-api`）+ FastAPI 風の `veil` パッケージ（引数束縛・pydantic 任意・
  `body_constraints`・HTTPException・OpenAPI）。既定 offload（GIL はハンドラ用プールで取る）。
- 単体 32 件（pydantic 有無）、`tests/e2e.sh`（2 例、HTTP/1.1・h2c、422、OpenAPI、SIGTERM / SIGINT）。zip の wheel を Debian 11（Python 3.9）でも確認。
