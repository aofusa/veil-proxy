# B-395: Proxy-Wasm の共有キューがプロセス全体で共有され、権限の無いモジュールが他のモジュールのメッセージを読み書きできた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx の 2 回目のセキュリティスキャン（`CLAUDE-SECURITY-20261114-024333`）の指摘 F27 から確認。

## 事象

- 共有キューのホスト関数はすべて、プロセスで 1 つの `SHARED_QUEUES` を使っていた。共有データ（`allow_shared_data`
  が要り、モジュール名ごとに分かれる）と違って権限を見ず、`vm_id` の引数は無視していた。
- 何も許可されていないモジュールが、任意の名前のキューを登録・解決し、あるいは連番のキューの番号を順に試して、
  他のモジュールのキューからメッセージを取り出し（読み取って消し）、偽のメッセージを積めた。

## 対応

- キューは登録したモジュール（Proxy-Wasm の `vm_id`。veil ではモジュール名）のもの。名前の表は（持ち主, 名前）で引く。
- すべてのホスト関数に `allow_shared_data` を求める（無ければ `PROXY_RESULT_NOT_ALLOWED`）。
- `proxy_resolve_shared_queue` は `vm_id` の持ち主のキューを引き（空なら自分）、引いたモジュールを覚える。積めるのは
  登録・解決したモジュールだけ（他は `NOT_FOUND`）、取り出せるのは持ち主だけ（他は `EMPTY`）。
- 利用者ガイド（`wasm.md`・`ja` の `allow_shared_data`）と `examples/config.toml` に書いた。

## テスト

- `tests/proxy_wasm_conformance.rs` の `shared_queues_are_scoped_to_their_owner`。
