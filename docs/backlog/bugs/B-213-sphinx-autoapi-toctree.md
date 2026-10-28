# B-213: Python の API 文書（Sphinx）のビルドが sphinx-autoapi 3.8 で `toc.not_included` の警告になり CI が落ちる

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の PR の CI（`ci.yml` の `python docs + stubs (F-239)`、`sphinx-build -W`。2026-10-27）。

## 事象

`crates/veil-python/docs/api/index.rst: WARNING: document isn't included in any toctree`。
`requirements.txt` は sphinx-autoapi を `>=3.3` で指定しており、CI が 3.8.1 を入れた。3.8 は `autoapi_add_toctree_entry = True` でも
MyST（`.md`）の root の toctree へ `api/index` を足さない。

## 修正

`autoapi_add_toctree_entry = False` にし、`index.md` に「API reference」の toctree を置いて生成されるパッケージごとの
`api/veil/index`・`api/veil_kit/index` を書く（自動の挿入に頼らない）。sphinx-autoapi 3.8.1 で `sphinx-build -W` が通ることを確かめた。
