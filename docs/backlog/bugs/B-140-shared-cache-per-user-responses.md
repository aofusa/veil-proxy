# B-140: 応答のキャッシュが、利用者ごとの応答をほかの利用者へ返していた

**状態: 完了（feat/v080-release、2026-10-17。F-236 の監査の F7）**

## 症状

`[route.cache]` は、キャッシュのキーを (メソッド, ホスト, パス, クエリ, `key_headers`) で作り、保存では `no-store`・
`private` だけを断っていた。

- `Authorization` の付いた要求の応答も保存し、ほかの利用者へ返した（RFC 9111 §3.5 に反する）。
- `Set-Cookie` を持つ応答も保存し、セッションのクッキーをほかの利用者へ配った。
- 応答の `Vary` を、照合では見ていなかった（`Vary: Accept-Encoding` の gzip の応答を、gzip を受けない利用者へ返す等）。

## 修正

- `Authorization` の付いた要求は、キャッシュを引かず、保存もしない。
- `Set-Cookie` を持つ応答は保存しない。
- `respect_vary`（既定 true）のとき、`Vary` が挙げるヘッダが全部 `key_headers` に入っていなければ保存しない
  （`Vary: *` は今まで通り保存しない）。

## 試験

`cache::manager::tests::test_store_rejects_per_user_responses`。

## 関連

F-236。
