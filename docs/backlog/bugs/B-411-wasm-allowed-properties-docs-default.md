# B-411: `allowed_properties` の既定を文書が「空なら全部読める」と書いていた（実装は何も読めない）

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

## 事象

B-410（例の WAF の許可リスト）を直す途中で見つけた。

- `examples/config.toml` は `allowed_properties` を「空 = 全許可」と書き、利用者ガイド（`docs/guide/wasm.md`・`ja`）の
  `data_files` の節も「モジュールに `allowed_properties` を書くなら `"plugin_data.*"` を含める」（書かなければ読める）と読める
  書き方だった。
- 実装（`ModuleCapabilities::is_property_allowed`）は空なら何も許さない（既定で拒否。構造体の説明どおり）。許していない
  プロパティは `NotFound` で返るので、ガイドの例どおりに `data_files` を設定したフィルタは `plugin_data.<名前>` を読めなかった。
- ガイドの capability の表に `allowed_properties`・`allow_property_write` が無く、veil が用意するプロパティの一覧も無かった。

## 修正

文書を実装（既定で拒否）に合わせた。安全側の既定なので実装は変えない。

- `examples/config.toml`: 空（既定）なら何も読めない、`data_files` は `"plugin_data.*"` 等で許す。
- 利用者ガイド（英日）: `data_files` の例に `allowed_properties = ["plugin_data.*"]` を足した。capability の表に
  `allowed_properties`（書き方・空なら何も読めない・許していないものは無いものとして返る・用意するプロパティの一覧）と
  `allow_property_write` を足した。
