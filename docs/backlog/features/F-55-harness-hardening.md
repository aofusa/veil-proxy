# F-55: container_security ハーネス堅牢化

## 目的

CI/nightly で **フレーク・ハングなく** グリーン/レッド判定できるオーケストレータにする。

## 現状

- SIGHUP 後の `/__metrics` + health 二段ポーリング（`poll_reload_ready`）
- カオスバックグラウンドジョブに `timeout` 適用
- `check_kernel_capabilities` — io_uring/kTLS 検出、`KERNEL_REQUIRE_IO_URING=1` で chaos スキップ
- `lib/report.sh` — `suite_summary.json` / `suite_summary_junit.xml` 集約

## 改修内容（残件）
- GitHub Actions matrix（glibc/musl）統合

## 受け入れ条件

- 負荷 CI 環境でも SIGHUP カオスが安定
- 非対応カーネルではスキップ理由をログに記録して exit 0