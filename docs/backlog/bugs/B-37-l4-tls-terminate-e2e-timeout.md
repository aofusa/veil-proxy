# B-37: L4 TLS 終端リスナーが E2E で応答しない

## 事象

`[l4]` `tls = "terminate"` リスナー（8446）へ HTTPS 接続するとタイムアウト。平文 upstream（echo 9008）への転送が完了しない。

## 再現

1. `./tests/e2e_setup.sh start`
2. `curl -ks --http1.1 https://127.0.0.1:8446/ -m 5` → タイムアウト

L4 passthrough（8444）・least_conn（8445）は正常。

## 影象

- `test_e2e_l4_tls_terminate_forward` が失敗（現状 `#[ignore]`）

## 改修案

`src/l4/` の TLS 終端ハンドシェイク・平文 upstream 接続経路を調査。証明書ロード・ALPN・upstream 接続タイムアウトを確認。

## 対応状況

完了（2026-07-08）: `src/l4/proxy.rs` に TLS 終端実装（サーバー TLS ハンドシェイク + 平文 upstream 双方向転送）。`test_e2e_l4_tls_terminate_forward` の `#[ignore]` を解除。