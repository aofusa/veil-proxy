# F-68: リソース枯渇（Resource Exhaustion）テスト

出典: `security_chaos_fuzzing_report.md` §2.2.2。親: [F-53](F-53-chaos-engineering-expansion.md)。

## 目的

CPU・メモリ・ファイルディスクリプタ・PID を極端に絞った環境下で Veil が
Graceful Degradation（接続拒否/エラー応答）に留まり、panic/OOM-Kill/ハングを
起こさないことを検証する。

## 実装済み

- `tools/container_security/chaos/resource_exhaustion_chaos.sh`（既定 `SKIP_RESOURCE_EXHAUSTION=1`）。
  - cgroup 制約付きで専用 Veil を起動（`--memory 48m` / `--cpus 0.25` /
    `--pids-limit 64` / `--ulimit nofile=256` / 小 tmpfs）。
  - `wrk -c400` の高並行負荷を印加し、コンテナ状態（OOMKilled/ExitCode）と
    ログの panic/abort 痕跡を検査。
- `run.sh` に配線（`report.sh` に `resource_exhaustion` 追加）。

## 残件

- io_uring の SQ/CQ リング溢れを狙った専用シナリオ（現状は汎用 cgroup 圧迫のみ）。
- 制約値のスイープ（メモリ段階的減少での閾値探索）と、
  「起動失敗」と「稼働中枯渇」の切り分けレポート。
- 実行して顕在化した問題があれば bugs/ に起票（本 PR 時点では未実行＝環境負荷回避のため）。

## 受け入れ条件

- `SKIP_RESOURCE_EXHAUSTION=0` で実行し、制約下でも panic/segfault 痕跡が無く、
  過負荷分は接続拒否/エラー応答で吸収されること。
