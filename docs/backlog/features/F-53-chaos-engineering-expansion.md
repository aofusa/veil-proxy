# F-53: カオスエンジニアリング拡充

## 目的

circuit breaker、retry、buffering、graceful shutdown の **障害下生存性** をコンテナ環境で検証する。

## 現状

- `tools/container_security/chaos/toxiproxy_setup.sh` — Toxiproxy 2.9（API :8474、proxy :8480）+ whoami backend
- `tools/container_security/harness/scripts/toxiproxy_chaos.sh` — 遅延注入・回復検証
- `prepare_veil_test_config()` — Landlock 下の DNS 制約を回避するため Toxiproxy 上流を起動時 IP に置換
- `fixtures/veil-config.toml` — Landlock に DNS/NSS 用パス（`/etc/resolv.conf`、`/lib`、`/usr` 等）を追加
- 既存: 高並行負荷、SIGHUP リロード、接続チャーン
- `circuit_breaker_chaos.sh` — limit_data toxic + 5xx カウントで CB 発火検証
- `slowloris_chaos.sh` — 部分リクエスト backpressure 検証
- `toxiproxy_chaos.sh` — limit_data による接続リセット検証を追加
- テスト設定: `toxi-pool` upstream + circuit breaker + prometheus

### 解決済み（2026-07-02）

- **502 原因**: Landlock 有効時に `veil-sec-toxiproxy` の DNS 解決が失敗（`Temporary failure in name resolution`）
- **対策**: ランタイム設定で上流 URL を Toxiproxy コンテナ IP に置換 + Landlock read_paths 拡張

## 完了（2026-07-06）

すべての改修内容が実装済み、または子チケットで完了した:

- **Pumba（pause/kill/netem）・`tc` によるパケットロス** → [F-69](F-69-pumba-network-kernel-chaos.md)（完了）。
  loss/delay/dup/corrupt + reorder + 複合（loss+delay 同時）を実装。
- **Backend 障害時の circuit breaker 発火・fallback 検証** → `circuit_breaker_chaos.sh`（完了）。
  バックエンドのプロトコル違反耐性は子 [F-67](F-67-backend-protocol-violation-tests.md)（完了）。
- **slowloris / 部分 body による backpressure 検証** → `slowloris_chaos.sh`（完了）。
- **リソース枯渇** → 子 [F-68](F-68-resource-exhaustion-tests.md)（完了）。io_uring SQ/CQ 飽和調査で
  [B-24](../bugs/B-24-sq-full-future-hang.md)（I/O Future 永久ハング）を検出・修正。
- Chaos Mesh / Chaos Blade（K8s 移行後）は本プロジェクトの K8s 移行が前提のため対象外
  （移行時に別チケットで再検討）。

## 受け入れ条件

- [x] Toxiproxy 遅延下でも Veil がクラッシュせず、遅延解除後に回復（`toxiproxy_chaos.sh`）。
- [x] カオスフェーズにタイムアウトがあり CI がハングしない（各スクリプトに timeout / 既定 SKIP）。