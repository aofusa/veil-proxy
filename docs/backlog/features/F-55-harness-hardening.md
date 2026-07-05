# F-55: container_security ハーネス堅牢化

## 目的

CI/nightly で **フレーク・ハングなく** グリーン/レッド判定できるオーケストレータにする。

## 現状

- SIGHUP 後の `/__metrics` + health 二段ポーリング（`poll_reload_ready`）
- カオスバックグラウンドジョブに `timeout` 適用
- `check_kernel_capabilities` — io_uring/kTLS 検出、`KERNEL_REQUIRE_IO_URING=1` で chaos スキップ
- `lib/report.sh` — `suite_summary.json` / `suite_summary_junit.xml` 集約

## 実装済み（2026-07-05）
- **GitHub Actions matrix（glibc/musl）統合**: `.github/workflows/container-security-nightly.yml`
  で glibc/musl マトリクスで `run.sh` を実行し、`results/`（`suite_summary.json` /
  `suite_summary_junit.xml` 含む）を `if: always()` で artifact 保存、サマリを Job Summary へ
  出力（[F-57](F-57-container-security-ci.md)）。`fail-fast: false` で片方の失敗が他方を止めない。

## 改修内容（残件）
- なし（本チケットの残件は解消。個別ツールの CI 有効化は F-57 で管理）。

## 受け入れ条件

- 負荷 CI 環境でも SIGHUP カオスが安定
- 非対応カーネルではスキップ理由をログに記録して exit 0