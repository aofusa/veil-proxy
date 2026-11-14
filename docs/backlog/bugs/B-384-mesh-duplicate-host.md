# B-384: メッシュの HTTP/1.1 が複数の `Host` を受け、最後の値で認可して全部を上流へ送っていた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx の 2 回目のセキュリティスキャン（`CLAUDE-SECURITY-20261114-024333`）の指摘 F8 から確認。

## 事象

- `mesh::http1` の要求の走査は、`host` の行ごとに `authority` を上書きしていた（最後の値が勝つ）。その値で
  仮想ホストを選び、RBAC（`:authority`・`hosts`・`notHosts`）・JWT の規則もその値を見る。
- 上流へのヘッダ（`request_head`）は元の `host` の行をすべてそのまま書いていた。最初の `Host` を使う上流は、
  プロキシが認可したホストとは別のホストの応答を返す。
- RFC 9112 §3.2 は、`Host` が複数ある要求に 400 を返すことを求める。
- データプレーンの TOML の経路（`proxy::h1`）は `Host` を 1 つだけ書くので当たらない。

## 対応

- 要求のヘッダの走査を同期関数 `scan_request_headers`（結果は `ReqHeadScan`）へ切り出し、2 つ目の `host` で
  `None`（400）を返す。本文の区切りの検査（B-315）も同じ関数で行う。

## テスト

- `mesh::http1::tests::scan_request_headers_rejects_duplicate_host`。
- メッシュは `full` に入らない（`--features full,mesh-istio,mesh-linkerd` で単体試験を回した）。kind での E2E は
  回していない。
