# B-245: B-231（allowed_ips が空なら loopback だけ）の後、container_security の admin_security・metrics_probe が 403 で失敗し、metrics_host_label_bounded は届かないまま PASS と報告していた

**状態: 修正済み（v0.10.0）**

## 出典

2026-10-27 の毎晩の container_security（run 40525405501、feat/v0100 c33710a3）。glibc・musl・sanitizers・chaos-extended の 4 ジョブすべてで
`admin_security`・`full_features_security`（中で admin_security を回す）・`metrics_probe` が failed。

## 事象

- B-231 で `[admin]`・`[prometheus]` の `allowed_ips` が空なら loopback だけを許すようにした。`tools/container_security/fixtures/veil-config.toml`
  は `allowed_ips` を書いておらず、プローブは別のコンテナ（Docker のネットワーク）から叩くので、管理 API もメトリクスも 403 になった
  （`admin_security`: `expect=401 got=403` ほか 5 件、`metrics_probe`: `metrics_ok: 0/100`）。
- `metrics_probe.sh` の `metrics_host_label_bounded` は、系列を 1 本も読めない（`before=0 after=0`）ときも「増えていない」として PASS と
  報告していた（照合できていないのに成功と見せる。B-217 と同じ種類）。

## 修正（v0.10.0）

- フィクスチャの `[prometheus]`・`[admin]` に `allowed_ips = ["127.0.0.1", "::1", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"]` を書いた。
- `metrics_probe.sh`: 計測後の系列が 0 本なら `metrics_host_label_bounded` を FAIL にする。

Helm（`metrics.allowedIps` の既定はポッドのネットワーク）・`deploy/kubernetes/base`・`examples/grafana`・`tools/perf` の設定は
`allowed_ips` を明示しているので影響しない。

## 受け入れ条件

- 毎晩の container_security で `admin_security`・`full_features_security`・`metrics_probe` が passed。
