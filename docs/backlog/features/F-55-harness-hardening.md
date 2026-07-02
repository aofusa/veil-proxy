# F-55: container_security ハーネス堅牢化

## 目的

CI/nightly で **フレーク・ハングなく** グリーン/レッド判定できるオーケストレータにする。

## 現状

- SIGHUP 後のヘルスをポーリング待ち（固定 sleep 排除）
- カオスバックグラウンドジョブに `timeout` 適用
- `check_kernel_capabilities` による io_uring/kTLS の条件付きスキップ

## 改修内容（残件）

- `/metrics` または Admin API によるリロード完了検知
- JUnit/JSON レポートの統合アーティファクト
- GitHub Actions matrix（glibc/musl）統合

## 受け入れ条件

- 負荷 CI 環境でも SIGHUP カオスが安定
- 非対応カーネルではスキップ理由をログに記録して exit 0