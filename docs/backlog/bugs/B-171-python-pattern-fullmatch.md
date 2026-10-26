# B-171: Python の `body_constraints` の `pattern` が文字列全体との一致を要求する（JSON Schema・pydantic は部分一致）

**状態: 修正済み（v0.10.0）**

## 出典

F-268 の `Query(pattern="^a")` の確認で、`"ab"` が 422 になった（2026-10-26）。

## 事象

- `crates/veil-python/python/veil/__init__.py` の制約の検査（`body_constraints` と、pydantic が無いときの F-268 の
  `Query()`・`Header()` 等の検査）が `re.fullmatch` を使っていた。
- JSON Schema の `pattern` は暗黙に固定されない（ECMA-262 の正規表現で探す）。pydantic v2・FastAPI も部分一致
  （`^`・`$` を自分で書く）。`pattern: "^a"` で `"ab"` が通らず、pydantic の有無で結果が変わっていた。

## 修正

- `re.search` にした（`^`・`$` は利用者が書く。OpenAPI に出る `pattern` と意味が一致する）。

## 試験

- `crates/veil-python/tests/test_fastapi_compat.py` の `test_pattern_is_a_search`（pydantic の有無の両方）。
