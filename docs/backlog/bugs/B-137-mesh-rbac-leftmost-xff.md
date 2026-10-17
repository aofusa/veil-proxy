# B-137: メッシュの RBAC の remote_ip が、X-Forwarded-For の左端（クライアントが書ける）を使っていた

**状態: 完了（feat/v080-release、2026-10-17。F-236 の監査の F3）**

## 症状

RBAC の `remote_ip`（Istio の AuthorizationPolicy の `remoteIpBlocks`）は、X-Forwarded-For の左端の値で判定していた。
左端はクライアントが自由に書けるので、`X-Forwarded-For: 10.0.0.1` を付けるだけで IP の規則を回避できた。

## 修正

Envoy の「元のクライアントのアドレス」と同じ規則にした（`l7::client_address`）。

- `use_remote_address` あり: 接続の相手。`xff_num_trusted_hops` が N なら X-Forwarded-For の右から N 番目。
- なし: X-Forwarded-For の右端から `xff_num_trusted_hops` だけ左。
- ヘッダが無い・足りない・読めないときは接続の相手。左端は使わない。

HCM の `xff_num_trusted_hops`（タグ 19）を xDS から読むようにした。

## 試験

`mesh::l7::tests::client_address_never_uses_leftmost_xff`。

## 関連

F-236、F-215〜F-225（メッシュ）。
