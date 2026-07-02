# F-53: カオスエンジニアリング拡充

## 目的

circuit breaker、retry、buffering、graceful shutdown の **障害下生存性** をコンテナ環境で検証する。

## 現状

- `tests/container_security/chaos/toxiproxy_setup.sh` — Toxiproxy 2.9（API :8474、proxy :8480）+ whoami backend
- `tests/container_security/harness/scripts/toxiproxy_chaos.sh` — 遅延注入・回復検証
- `prepare_veil_test_config()` — Landlock 下の DNS 制約を回避するため Toxiproxy 上流を起動時 IP に置換
- `fixtures/veil-config.toml` — Landlock に DNS/NSS 用パス（`/etc/resolv.conf`、`/lib`、`/usr` 等）を追加
- 既存: 高並行負荷、SIGHUP リロード、接続チャーン

### 解決済み（2026-07-02）

- **502 原因**: Landlock 有効時に `veil-sec-toxiproxy` の DNS 解決が失敗（`Temporary failure in name resolution`）
- **対策**: ランタイム設定で上流 URL を Toxiproxy コンテナ IP に置換 + Landlock read_paths 拡張

## 改修内容（残件）

- Pumba（pause/kill/netem）、`tc`/`iptables` によるパケットロス
- Backend 障害時の circuit breaker 発火・fallback 検証用フィクスチャ
- slowloris / 部分 body による backpressure 検証
- Chaos Mesh / Chaos Blade（K8s 移行後）

## 受け入れ条件

- Toxiproxy 遅延下でも Veil がクラッシュせず、遅延解除後に回復
- カオスフェーズにタイムアウトがあり CI がハングしない