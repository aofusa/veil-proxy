# tests/load — 負荷テストハーネス（F-56）

任意入力の不変条件（panic なし・決定性）は `tests/routing_proptest.rs` /
`tests/config_proptest.rs` の proptest で担保する。本ディレクトリはその **負荷版**で、
稼働中の Veil に高並行負荷をかけて **latency（p50/p90/p99）と error 率**を計測し、
`tools/container_security/` の chaos と併用して **chaos 前後の劣化を比較**する。

すべて docker のみで完結する（wrk / k6 の公式イメージを使用）。

## 構成

| ファイル | 役割 |
|----------|------|
| `run_load.sh` | wrk または k6 で負荷を印加し結果を `results/` に保存。`ENGINE=wrk\|k6`、`PHASE=baseline\|chaos` |
| `k6_load.js` | k6 スクリプト。error 率・p95 latency に **合否閾値**を設定（chaos 時の劣化を自動判定） |
| `compare.sh` | baseline / chaos の wrk レポートから主要指標を並べて差分を可視化 |

## 使い方

### 単発（baseline）

```bash
# wrk（latency 分布を出力）
TARGET_URL=https://127.0.0.1:443/ tests/load/run_load.sh

# k6（閾値で PASS/FAIL 判定）
ENGINE=k6 TARGET_URL=https://127.0.0.1:443/ \
  K6_MAX_ERROR_RATE=0.05 K6_MAX_P95_MS=1000 tests/load/run_load.sh
```

主なパラメータ（環境変数）: `CONNECTIONS`(200) / `THREADS`(4) / `DURATION`(20s) /
`K6_MAX_ERROR_RATE`(0.05) / `K6_MAX_P95_MS`(1000)。

### chaos 前後比較

`tools/container_security/` の chaos（toxiproxy 遅延・パケットロス・CB 発火等）と併用する。

```bash
# 1. 平常時のベースライン
PHASE=baseline tests/load/run_load.sh

# 2. chaos を注入（例: toxiproxy 遅延）
#    tools/container_security/harness/scripts/toxiproxy_chaos.sh 等

# 3. chaos 下の計測
PHASE=chaos tests/load/run_load.sh

# 4. 差分レポート
tests/load/compare.sh
```

`compare.sh` は Requests/sec・Latency 分布・Non-2xx・Socket errors を baseline/chaos で
並べて出力する。**受け入れ基準の目安**: chaos 解除後に error 率がベースライン水準へ回復し、
chaos 下でも Veil がクラッシュ・ハングしない（graceful degradation）こと。

## 注意

- `--network host` を使うため、ローカルで Veil を起動しておくこと（`./tests/e2e_setup.sh` 等）。
- 自己署名証明書のテスト環境では k6 の `insecureSkipTLSVerify` / wrk の TLS 検証スキップを利用する。
  本番計測では必ず外すこと。
- 結果は `tests/load/results/`（gitignore 済み）に保存される。恒久保存する集計レポートは
  AGENTS.md の方針に従い `docs/artifacts/` に置く。
