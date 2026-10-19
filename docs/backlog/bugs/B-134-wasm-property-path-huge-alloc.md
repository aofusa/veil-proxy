# B-134: Proxy-Wasm の get_property が、ゲストの渡す値の数で巨大な確保をしていた

**状態: 完了（feat/v080-release、2026-10-17。OpenBSD の単体テストで発見）**

## 症状

`proxy_get_property` のパスは、長さ付きの形式（`[要素数:4][長さ:4][要素]...`）か NUL 区切り（SDK・Envoy の形式。B-131）で
来る。veil は先に長さ付きの形式として読み、先頭 4 バイトを要素数として `Vec::with_capacity(要素数)` を確保していた。

NUL 区切りの `plugin_root_id` は先頭が `plug`（約 17 億）になり、41GB を確保しようとする。

- Linux は overcommit で確保が通り、実際には触らないので気付かなかった。
- OpenBSD（x86_64・aarch64）は確保に失敗して abort し、単体テストのバイナリごと落ちた。

要素数は Wasm のモジュールが自由に渡せるので、確保が拒否される OS ではモジュールがホストのプロセスを落とせた。

## 修正

要素は少なくとも長さの 4 バイトを持つので、`(データ長 - 4) / 4` を超える要素数は長さ付きの形式ではないとして、確保の前に断る
（そのあと NUL 区切りとして読む）。同じ形の `abi::deserialize_headers` は、先にサイズ表の範囲を確かめていて問題なかった。

## 試験

`wasm::host::properties::tests::parse_len_prefixed_path_rejects_counts_larger_than_data`。OpenBSD の単体テスト（`--lib`）が通る。

## 関連

B-131、F-236（セキュリティの監査）。
