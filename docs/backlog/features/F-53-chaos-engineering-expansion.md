# F-53: カオスエンジニアリング拡充

## 目的

circuit breaker、retry、buffering、graceful shutdown の **障害下生存性** をコンテナ環境で検証する。

## 現状

- `tests/container_security/chaos/toxiproxy_chaos.sh` — Toxiproxy による upstream 遅延注入（docker のみ）
- 既存: 高並行負荷、SIGHUP リロード、接続チャーン

## 改修内容（残件）

- Pumba（pause/kill/netem）、`tc`/`iptables` によるパケットロス
- Backend 障害時の circuit breaker 発火・fallback 検証用フィクスチャ
- slowloris / 部分 body による backpressure 検証
- Chaos Mesh / Chaos Blade（K8s 移行後）

## 受け入れ条件

- Toxiproxy 遅延下でも Veil がクラッシュせず、遅延解除後に回復
- カオスフェーズにタイムアウトがあり CI がハングしない