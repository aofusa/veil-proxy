# F-194: Python バインディング（PyO3）— FastAPI 風デコレータ

**状態: 進行中（feat/f188-api-library-vds-k8s）**

## 目的

Python から `@veil.route` / `@app.get(...)` のデコレータでルートとハンドラを登録し、veil のプロトコル処理・I/O 性能を
手軽に使えるようにする。Pydantic は任意（あれば本文の検証と OpenAPI に使う）。

## 設計

- 別クレート `crates/veil-python`（PyO3、abi3-py38、maturin、モジュール `veil._veil`）+ 純 Python の `veil` パッケージ。
- Python ハンドラは既定で offload（GIL を取って専用プールで実行）、`offload=False` でワーカー上で直接実行。
- 引数束縛は FastAPI と同じ規則（パスパラメータ・クエリ・本文・`req`）。`body_constraints` で pydantic 無しでも検証と OpenAPI 反映。

## 受け入れ条件

- wheel を venv にインストールしてサンプルを起動し、ルーティング・変換・検証（pydantic 有無）・OpenAPI を E2E で確認。
