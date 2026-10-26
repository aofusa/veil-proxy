# B-178: Proxy-Wasm の `plugin_data.<名前>` を読むたびにファイル全体を複製する

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 のレビュー（`docs/artifacts/v0100/review-api-wasm.md` の #11、2026-10-26）。

## 事象

- `src/wasm/host/properties.rs` の `get_property_value` は値を `Vec<u8>` で返し、`plugin_data.<名前>`（`data_files` で起動時に
  読んだファイル。IP リスト等で数 MB もあり得る）を `get_property` のたびに `d.to_vec()` で複製してからゲストのメモリへ写していた。
  要求ごとに読むフィルタではホットパスのアロケーション規則に反する。
- 同じ関数はパス・メソッド・クライアントの IP・プラグイン名・定数も毎回 `Vec` にしていた。`proxy_get_property` はパスを
  ゲストのメモリから `Vec` に複製し、`String` に組み立てていた。

## 修正

- `get_property_value` は `PropertyValue`（共有の `Arc<[u8]>`・`Arc<str>`・定数の `&'static [u8]`・その場で作った `Vec<u8>`）を返す。
  `plugin_data.<名前>` は `Arc` の参照を増やすだけで、ゲストのメモリへは 1 回だけ写す（ゲストの `proxy_on_memory_allocate` で
  確保した領域への書き込み）。
- `proxy_get_property` はパスをゲストのメモリから直接読み、`.` で繋いだパスを 128 バイトのスタックの領域に組み立てる
  （`PropertyPath`。長いパス・UTF-8 でない要素だけ `String` にする）。

## 試験

- `src/wasm/host/properties.rs` の単体テスト `plugin_data_is_shared_not_copied`（返した値がモジュールの `Arc` と同じ領域を指す）、
  `parse_path_into_matches_parse_path`（NUL 区切り・長さ前置き・空・不正な長さ・長いパス・UTF-8 でない要素で従来の
  `parse_path` と同じ結果）。
- F-271 の `data_files_are_readable_as_plugin_data_property`（フィルタが `plugin_data.dataset` を読んで返す）。

## 実施内容（2026-10-26）

- 上の修正と試験。
