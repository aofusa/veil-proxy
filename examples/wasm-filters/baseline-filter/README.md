# Baseline Filter

要求をフィルタだけで返す Proxy-Wasm フィルタ（F-271）。`type = "Respond"` のルートにモジュールを付ける使い方、
HTTP/1.1・HTTP/2・HTTP/3 の要求本文のフェーズ、`data_files`、`instance_reuse` の例で、`tests/fixtures/wasm/baseline_filter.wasm`
として E2E・適合試験でも使う。

## ビルド

```bash
cargo build --target wasm32-wasip1 --release
cp target/wasm32-wasip1/release/baseline_filter.wasm ../../../tests/fixtures/wasm/
```

## 動き

| パス | 応答 |
|------|------|
| `/baseline11?a=1&b=2` | クエリの値の和（`3`）。本文があれば本文の整数も足す（`on_http_request_body`） |
| `/dataset` | プロパティ `plugin_data.dataset`（`data_files` の `dataset` で読ませたファイル） |
| `/rewrite` | 要求本文を `rewritten:<本文>` に書き換えて上流へ送る |
| `/trap` | トラップする（使い回したインスタンスがトラップの後に作り直されることの確認用） |

パスの先頭の `/fresh` は無視する（同じ要求を別のモジュールのルートに振る試験用）。

フィルタが返す応答には `x-configure-count`（そのインスタンスで `on_configure` が走った回数）と `x-instance-seq`
（そのインスタンスが作った http context の数）が付く。`instance_reuse = true` なら `x-configure-count` は 1 のまま
`x-instance-seq` が増える。

## 設定例

```toml
[[wasm.modules]]
name = "baseline"
path = "/etc/veil/wasm/baseline_filter.wasm"
instance_reuse = true
data_files = { dataset = "/etc/veil/data/dataset.json" }

[wasm.modules.capabilities]
allow_send_local_response = true
allow_request_body_read = true
allow_request_body_write = true
allowed_properties = ["request.*", "plugin_data.*"]

[[route]]
modules = ["baseline"]
[route.conditions]
path = "/baseline11"
[route.action]
type = "Respond"
status = 404
```
