# B-490: HTTP/1.1 だけ拡張メソッドを 405 で断る（管理 API の PURGE が HTTP/1.1 で届かない）

**状態: 修正済み（feat/protocol-parity。2026-11-22 起票・修正。機能 × プロトコルの網羅性の確認）**

## 出典

機能とプロトコルの対応の確認（`docs/artifacts/protocol-parity/audit.md`）。B-483（管理 API を HTTP/3 でも受ける）の
3 プロトコルの試験で、HTTP/1.1 の `PURGE` が 405 になって見つけた。

## 事象

- HTTP/1.1（`proxy/h1.rs`）は要求のメソッドを 9 つ（GET・HEAD・POST・PUT・DELETE・CONNECT・OPTIONS・TRACE・PATCH）に
  限り、それ以外を管理 API・ルートの解決の前に 405 で断る。HTTP/2・HTTP/3 は `:method` がトークン（RFC 9110 §9.1）で
  あれば受け、ルートへ渡す。
- そのため:
  - 文書（`admin-api.md`）にある `PURGE <パス>` によるキャッシュの削除が HTTP/1.1 でだけ動かない（405）。
  - WebDAV（`PROPFIND`・`MKCOL` 等）など拡張メソッドを使う上流へ、HTTP/2・HTTP/3 では届くのに HTTP/1.1 では届かない。
- 405 はルートの `allowed_methods` の結果と区別が付かず、許可していない理由が設定から読み取れない。

## 対応（2026-11-22）

- HTTP/1.1 も HTTP/2・HTTP/3 と同じく、トークンであるメソッドを全部受ける（固定の 9 つの表を外した。トークンかどうかは
  httparse が確かめており、トークンでないメソッドは従来どおり 400）。メソッドで絞るのはルートの
  `[route.security] allowed_methods`（3 プロトコル共通で 405）。
- 平文のリスナーのプロトコルの判別（`proxy::detect`）も、9 つのメソッドの先頭 4 バイトではなく「メソッドのトークン + SP」で
  HTTP/1.1 と判別する（拡張メソッドの接続は応答無しで閉じていた。h2c のプリフェースの途中は除く）。
- 文書: `routing.md`（英日）の `allowed_methods` に、書かなければ 3 プロトコルともトークンのメソッドを全部受けることを書いた。
- E2E（`test_error_handling_invalid_method`）は、トークンでないメソッド（`INV@LID`）が 400 になることを確かめる形に改めた
  （トークンの `INVALID` は上流へ渡る）。

## 受け入れ条件

- 自己完結型 E2E（`b490_extension_methods_on_every_protocol`）: `PROPFIND` が HTTP/1.1・HTTP/2（h2c・TLS）・HTTP/3 の
  どれでも上流へ届き、`allowed_methods = ["GET"]` のルートでは 3 プロトコルとも 405。
- `b483_admin_api_on_every_protocol`: 管理 API の `PURGE` が HTTP/1.1 でも管理 API に届き、上流へ流れない。

## 関連

- [B-483](B-483-admin-api-not-served-over-http3.md)
