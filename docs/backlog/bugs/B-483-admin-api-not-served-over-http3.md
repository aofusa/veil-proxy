# B-483: 管理 API（`/__admin/*`）とキャッシュの PURGE が HTTP/3 で効かない

**状態: 修正済み（feat/protocol-parity。2026-11-21 起票・2026-11-22 修正。機能 × プロトコルの網羅性の確認）**

## 出典

機能とプロトコルの対応の確認（`docs/artifacts/protocol-parity/audit.md` の #3）。

## 事象

- `[admin]`（`admin` feature）の管理 API（`GET /__admin/config`・`stats`・`vds`、`POST /__admin/reload`・`tls/reload`、
  `/__admin/cache/purge`）と `PURGE` メソッドは、HTTP/1.1（`proxy/h1.rs`）と HTTP/2・h2c（`h2_admin_response`）では
  ルートの解決の前に受けるが、HTTP/3 には処理が無い。
- HTTP/3 では同じ要求が普通のルートへ流れる。全部を受けるプロキシのルートがあれば、管理用の `Authorization: Bearer`
  ごと上流へ送られる（管理のトークンが上流に漏れる）。HTTP/3 を使うクライアント（Alt-Svc を覚えたブラウザ・
  `curl --http3`）からは管理 API が使えない。

## 改修案

- HTTP/3 の要求の分類（`classify`）・バッファ経路の入口で、メトリクスのエンドポイントと同じ位置に管理 API を受ける。
  HTTP/2 の `h2_admin_response`（状態・ヘッダ・本文を返す）を共有の場所へ移して HTTP/3 からも呼ぶ。
- 管理 API の要求ごとの費用は `[admin] enabled` の分岐だけ（無効なら比較もしない）。

## 対応（2026-11-22）

- 管理 API の判定と本文を `proxy::admin_response`（`is_admin_request` で振り分け）にまとめ、HTTP/2（`h2_admin_response`
  はその包み）と HTTP/3 で共有した。HTTP/3 は分類で管理 API の要求をバッファ経路へ回し、バッファ経路の入口（経路の解決の
  前）で応える。HTTP/1.1 は従来の実装のまま（規則は同じ）。
- HTTP/2 の下見でも管理 API の要求を溜める経路へ回すようにした（本文を HEADERS と別に送る `POST /__admin/reload` は
  ストリーミングの経路へ入り、管理 API を見ずに上流へ送られていた）。
- HTTP/1.1 の `PURGE` は B-490 で届くようになった（以前は 405）。
- 文書: `admin-api.md`（英日）に 3 プロトコルで同じに応え、上流へ届かないことを書いた。
- 自己完結型 E2E: `b483_admin_api_on_every_protocol`（4 経路で `GET /__admin/stats` が 200・トークン無しで 401、`PURGE`、
  本文を後から送る `POST /__admin/reload` が 200、管理の要求が上流へ届かない）。

## 受け入れ条件

- E2E または統合: HTTP/3 で `GET /__admin/stats`（正しいトークン → 200、無し → 401、許可外の IP → 403）、`PURGE` が
  HTTP/1.1・HTTP/2 と同じ結果。管理の要求が上流へ届かない。
- 文書（admin-api、英日）に「HTTP/1.1・HTTP/2・HTTP/3」と書く。

## 関連

- [B-308](B-308-websocket-shutdown-metrics-docs-h3-connect.md)（HTTP/3 の素の CONNECT が上流へ流れていた件と同じ型）
