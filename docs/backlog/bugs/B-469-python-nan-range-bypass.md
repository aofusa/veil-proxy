# B-469: Python の数の範囲の検査（ge・gt・le・lt、minimum・maximum）を NaN が通っていた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Numeric range validation (ge/gt/le/lt, minimum/maximum) is bypassed by NaN」
から確認。

## 事象

- `veil`（Python）の `_check_value` は範囲の外にあるかを `v < minimum` 等で見ていた。NaN はどの比較も偽なので、
  `Query(gt=0, le=1000)` の float の引数（`?amount=nan`）・`body_constraints` の number（`{"amount": NaN}`。
  `json.loads` は NaN を受ける）が検査済みとしてハンドラへ渡った。

## 対応

- 範囲の内側にあることを確かめる形（`not (v >= minimum)` 等）にして、NaN を範囲の外として 422 にする（pydantic と同じ）。
  範囲の無い float の NaN・本文の NaN の字句は従来どおり受ける（FastAPI と同じ）。

## テスト

- `crates/veil-python/tests/test_binding.py` の `TestConstraints.test_nan_is_out_of_range`（本文・クエリ）。
