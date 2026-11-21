# B-483: 管理 API（`/__admin/*`）とキャッシュの PURGE が HTTP/3 で効かない

**状態: 未着手（2026-11-21 起票。機能 × プロトコルの網羅性の確認、feat/protocol-parity）**

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

## 受け入れ条件

- E2E または統合: HTTP/3 で `GET /__admin/stats`（正しいトークン → 200、無し → 401、許可外の IP → 403）、`PURGE` が
  HTTP/1.1・HTTP/2 と同じ結果。管理の要求が上流へ届かない。
- 文書（admin-api、英日）に「HTTP/1.1・HTTP/2・HTTP/3」と書く。

## 関連

- [B-308](B-308-websocket-shutdown-metrics-docs-h3-connect.md)（HTTP/3 の素の CONNECT が上流へ流れていた件と同じ型）
