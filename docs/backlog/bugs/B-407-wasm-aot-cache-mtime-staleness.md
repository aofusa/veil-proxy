# B-407: WASM の AOT キャッシュを mtime だけで新しいと判断していたので、古い版のモジュールを戻すと新しい版のネイティブのコードを読んだ

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

## 事象

B-406（WAF の例のフィクスチャ）を調べる途中で見つけた。

`[[wasm.modules]] path` の `.wasm` は、隣の `<path>.cwasm`（Pulley は `<path>.pulley.cwasm`）の mtime が `.wasm` 以降なら、
そのキャッシュを `deserialize` して使っていた（F-36）。`.wasm` の中身は見ていなかった。

古い版の `.wasm` を mtime ごと戻すと（`cp -p`・tar の展開・パッケージの版下げ・バックアップからの復元・`git checkout` の後の
`touch -r` 等）、`.wasm` の mtime は新しい版から作ったキャッシュより古いので、新しい版のネイティブのコードを読んでいた。設定と
ファイルは古い版なのに、動くのは新しい版のフィルタになる（直したつもりの WAF の規則が効かない・戻したつもりの変更が残る）。
E2E の準備（`tests/e2e_setup.sh`）も `cp -p` で `.wasm` を写すので、手元の `tests/wasm/` と追跡している `tests/fixtures/wasm/`
の版が違うと、どちらの版で試験したのかが mtime の前後で決まっていた。

## 修正

- キャッシュの名前に `.wasm` の中身の印（xxh3-128 の 32 桁の 16 進）を入れた: `<path>.<印>.cwasm`・`<path>.<印>.pulley.cwasm`。
  `.wasm` を 1 回読んで印を取り、キャッシュが無ければ同じバイト列をコンパイルする（読んでからコンパイルまでの間に `.wasm` が
  替わっても、印とキャッシュの中身がずれない）。mtime は見ない。
- 新しいキャッシュを書けたら、同じ `.wasm` の同じ種類の形の名前で印の違うキャッシュを消す（版を上げるたびに溜まらない）。
  以前の `<path>.cwasm` は読まないし消さない（設定で明示した `.cwasm` かもしれない）。Pulley とネイティブのもう一方も残す。
- wasmtime の版・設定が違うキャッシュは、従来どおり `deserialize` が失敗してコンパイルし直す。
- 利用者ガイド（`docs/guide/wasm.md` と `ja`）に名前と、以前の名前のファイルは消してよいことを書いた。

## 試験

単体 `wasm::registry::tests::b407_restoring_an_older_module_does_not_load_the_newer_cache`（v1 を古い mtime で置いて読み、v2 に
替えて読み、v1 を古い mtime ごと戻して読む。v1 の関数が見え、前の版のキャッシュが消え、以前の名前のファイルは残る）、
`b407_aot_cache_name_pattern`（消す名前の形）。
