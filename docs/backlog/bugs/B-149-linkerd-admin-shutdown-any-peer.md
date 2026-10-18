# B-149: Linkerd の管理ポートの `/shutdown`・`/env.json` を、どの相手にも返していた

**状態: 完了（feat/v080-release、2026-10-18。F-236 の再走査の F8）**

## 症状

管理ポートは `0.0.0.0:4191` で待ち、`LINKERD2_PROXY_SHUTDOWN_ENDPOINT_ENABLED` が真なら `POST /shutdown` で相手を確かめずに
停止していた。`/env.json` も `LINKERD2_PROXY_*` を全部返した。クラスタの中のどのポッドからでもサイドカーを止め・設定を読めた。

## 修正

`route` に接続の相手を渡し、`/shutdown`・`/env.json` はループバック（IPv4 射影のループバックを含む）以外に 403 を返す
（linkerd2-proxy と同じ）。`/ready`・`/live`・`/metrics` は従来どおり。

## 試験

`mesh::linkerd::admin::admin_access_tests::shutdown_and_env_are_loopback_only`。

## 関連

F-236、F-215〜F-225。
