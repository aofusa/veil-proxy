# B-131: Proxy-Wasm の C++ SDK（emscripten）製のモジュールが property を読めずに異常終了する

**状態: 完了（feat/mesh-dataplane-impl、2026-10-14。F-222 の Istio の WasmPlugin（basic_auth）の試験で発見）**

## 症状

Istio の WasmPlugin のタスクの `basic_auth`（proxy-wasm-cpp-sdk 製）を veil で動かすと、各要求で
`CRITICAL: proxy_get_property("plugin_root_id", ...)` を出して `proc_exit` し、フィルタが効かない
（veil の既定ではエラーを無視して続行するので、認証が素通りになる）。

## 原因

ホスト側の 2 つの非互換。どちらも Rust SDK 製の既存の試験用モジュールが property を読まないため見えていなかった。

1. `proxy_get_property` のパスを独自の長さ付きの形式（`[要素数][長さ][要素]...`）でしか読まなかった。Proxy-Wasm の
   SDK（Rust・C++）と Envoy は要素を NUL で区切る（`request\0path`）。SDK からの呼び出しはすべて
   `PROXY_RESULT_PARSE_FAILURE` になっていた。
2. ホストからゲストへ値を渡すときのメモリ確保を `proxy_on_memory_allocate` のエクスポートだけで探していた。C++ SDK
   （emscripten）は `malloc` をエクスポートする（Envoy は両方を探す）。
3. `plugin_root_id` がルートコンテキストの数値 ID を返していた。仕様では設定の root_id（文字列）。C++ SDK はこの
   名前でルートコンテキストのファクトリを引くので、数値を返すとフィルタが作られない。

## 修正

1. 長さ付きの形式で読めなければ NUL 区切りとして読む（従来の形式も受け付ける）。
2. `proxy_on_memory_allocate` が無ければ `malloc` を使う（4 か所のヘルパ）。
3. モジュールに root_id を設定できるようにした（`FilterEngine::set_root_id`、`mesh` feature のみ。メッシュの
   WasmPlugin が ECDS の `PluginConfig.root_id` を渡す）。未設定なら従来どおり。

## 試験

- 単体: `wasm::host::properties::tests::parse_path_accepts_sdk_nul_separated_paths`・
  `parse_path_keeps_length_prefixed_paths`。
- `tools/k8s/mesh-e2e.sh veil wasm`（Istio の basic_auth: 資格情報なし 401・正しい資格情報 200・誤り 401・削除で 200）。
