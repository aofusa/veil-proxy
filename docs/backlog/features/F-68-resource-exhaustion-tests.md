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

## 追加実装（2026-07-06）

- **io_uring SQ/CQ リング飽和の専用調査で [B-24](../bugs/B-24-sq-full-future-hang.md) を検出・修正。**
  SQ 満杯時に全 I/O Future（Accept/Connect/Read/Write/SendMsg/Poll 系/Splice/Sleep）が
  SQE 未投入のまま `submitted=true` にして永久ハングしていた（CQ が永久未着）。
  `ring.rs` に `get_sqe_or_submit`（満杯時に pending を提出して SQ ヘッドを前進させ再取得）を
  追加し、全呼び出し箇所を「確保失敗時は `WouldBlock`（Sleep は満了扱い）で graceful に失敗」
  へ変更。回帰単体 `get_sqe_or_submit_drains_full_sq` を追加。これにより過負荷/CQ 溢れでも
  ハングせず graceful degradation に留まる（本チケットの主眼を **アプリ実行なしの単体テスト**で
  実証）。
- `resource_exhaustion_chaos.sh` に **メモリスイープ**（`RX_MEMORY_SWEEP`、既定
  `128m 96m 64m 48m 32m`）を追加し、各段で **起動失敗（startup=fail）と稼働中枯渇
  （runtime=oomkill/panic/ok）を切り分けてレポート**するよう拡張。panic/oomkill 検出時は
  findings として警告する。

## 残件

- 実 docker 環境での `SKIP_RESOURCE_EXHAUSTION=0` フル実行（外部インフラ・環境負荷のため
  CI nightly [F-82] 側で実施）。顕在化した問題は bugs/ に起票する運用。

## 受け入れ条件

- [x] io_uring SQ/CQ 飽和で panic/ハングしないこと（B-24 修正 + 回帰単体で実証）。
- [x] 制約値スイープと「起動失敗/稼働中枯渇」切り分けレポート（スクリプト実装済み）。
- `SKIP_RESOURCE_EXHAUSTION=0` の実 docker フル実行（環境依存のため CI nightly 側）。
