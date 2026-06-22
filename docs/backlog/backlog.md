# Veil バックログ（親ドキュメント）

機能追加・バグの **一覧・優先度・対応状況** をここで管理する。個別の説明・改修案は **専用の md ファイル** に書き、本ファイルでは **目次とステータスのみ** を保つ。

## 運用ルール（必須）

| 種別 | 格納先 | 各ファイルに含める内容（目安） |
|------|--------|--------------------------------|
| **機能追加** | [features/](features/) | 機能説明、現状、**改修内容**、**改修案**、受け入れ条件、依存・リスク |
| **バグ報告** | [bugs/](bugs/) | 事象（再現手順）、影響、**調査メモ**、**改修案**、関連コミット・PR |

**追加時**: 上記ディレクトリに md を新規作成し、**本ファイルの該当表に行を追加**する（優先度・対応状況・リンク）。

**修正完了時**: 該当チケットの **対応状況** を更新する。必要なら個別 md に「完了日・リリースタグ」を追記する。

優先度はリリース計画に応じて見直す。個別ドキュメント間の相互リンクは [features/](features/) 内の相対パスでよい。

---

## 優先度の目安

| 記号 | 意味 |
|------|------|
| **P0** | セキュリティ・データ損失・全面停止に直結 |
| **P1** | 本番運用の主要シナリオを阻害 |
| **P2** | 改善・拡張で価値が大きいが回避策あり |
| **P3** | 長期・調査寄り、フェーズ 2 以降でもよい |

---

## 対応状況の目安

| 状態 | 意味 |
|------|------|
| **未着手** | 仕様・調査のみ |
| **進行中** | 実装・検証中 |
| **完了** | マージ済み・リリース方針に従いクローズ |
| **保留** | 優先度下落、外部依存、方針未決 |

---

## 機能追加チケット一覧

| ID | 優先度 | 対応状況 | ドキュメント | 概要 |
|----|--------|----------|--------------|------|
| F-02 | P1 | 完了 | [features/e2e-test-hardening.md](features/e2e-test-hardening.md) | E2E の網羅・実装乖離の解消（369 テスト中 368～369 通過、負荷フレーキー1件を除き全通過） |
| F-03 | P1 | 完了 | [features/tls-cert-zero-downtime.md](features/tls-cert-zero-downtime.md) | 0 ダウンタイム TLS 証明書更新 |
| F-04 | P1 | 未着手 | [features/vds-xds-dynamic-config.md](features/vds-xds-dynamic-config.md) | 動的設定配信 API（VDS / xDS 相当） |
| F-06 | P1 | 完了 | [features/resilience-outlier-detection.md](features/resilience-outlier-detection.md) | サーキットブレーカー・リトライ・異常検知 |
| F-09 | P1 | 完了 | [features/prometheus-feature-flags.md](features/prometheus-feature-flags.md) | Prometheus 拡充と feature 無効化 |
| F-01 | P2 | 完了 | [features/grpc.md](features/grpc.md) | gRPC / gRPC-Web の完成度・テスト拡充 |
| F-05 | P2 | 未着手 | [features/acme.md](features/acme.md) | ACME 統合 |
| F-07 | P2 | 未着手 | [features/fuzzing-chaos-security.md](features/fuzzing-chaos-security.md) | ファジング・カオス・セキュリティスキャン |
| F-08 | P2 | 未着手 | [features/proxy-wasm-benchmarks.md](features/proxy-wasm-benchmarks.md) | Proxy-Wasm ベンチマーク |
| F-10 | P1 | 完了 | [features/opentelemetry.md](features/opentelemetry.md) | OpenTelemetry 対応 |
| F-18 | P1 | 未着手 | [features/l4-stream-proxy.md](features/l4-stream-proxy.md) | L4 (TCP/UDP) ストリームプロキシ |
| F-19 | P2 | 完了 | [features/advanced-load-balancing.md](features/advanced-load-balancing.md) | 高度なロードバランシング (Weighted, Consistent Hash等) |
| F-20 | P2 | 完了 | [features/proxy-cache-purge-advanced.md](features/proxy-cache-purge-advanced.md) | キャッシュのPurge機能・制御高度化 |
| F-21 | P2 | 完了 | [features/structured-access-log-admin.md](features/structured-access-log-admin.md) | 構造化アクセスログと管理Admin API |
| F-22 | P2 | 未着手 | [features/enhanced-health-check.md](features/enhanced-health-check.md) | ヘルスチェックの強化 (Active probing, TCP) |
| F-23 | P1 | 完了 | [features/refactor-cargo-features.md](features/refactor-cargo-features.md) | Cargo.toml の features フラグ整理 |
| F-24 | P2 | 未着手 | [features/F-24-zero-copy-pipeline-http2-http3.md](features/F-24-zero-copy-pipeline-http2-http3.md) | HTTP/2・HTTP/3 ゼロコピーパイプライン（splice / バッファ参照渡し） |
| F-25 | P1 | 進行中 | [features/F-25-seccomp-bpf-prot-exec-validation.md](features/F-25-seccomp-bpf-prot-exec-validation.md) | seccomp BPF 引数レベル検証（mprotect/mmap で PROT_EXEC をブロック） |
| F-26 | P2 | 進行中 | [features/F-26-http2-bytes-zero-copy.md](features/F-26-http2-bytes-zero-copy.md) | HTTP/2 ヘッダ/ボディのヒープ割り当て排除（bytes クレートゼロコピー化） |
| F-27 | P2 | 未着手 | [features/F-27-wasm-instance-pooling-async-fuel.md](features/F-27-wasm-instance-pooling-async-fuel.md) | WASM インスタンスプーリング + wasmtime Async/Fuel Yield |
| F-28 | P1 | 進行中 | [features/F-28-custom-iouring-impl.md](features/F-28-custom-iouring-impl.md) | monoio 削除・カスタム io_uring 実装（thread-per-core、IORING_REGISTER_RESTRICTIONS） |
| F-11 | P3 | 未着手 | [features/dashboard.md](features/dashboard.md) | ダッシュボード機能 |
| F-12 | P3 | 未着手 | [features/config-generator-webui.md](features/config-generator-webui.md) | config.toml ジェネレータ Web UI |
| F-13 | P3 | 未着手 | [features/documentation-site.md](features/documentation-site.md) | 公式ドキュメントサイト |

### フェーズ 2（機能安定後）

| ID | 優先度 | 対応状況 | ドキュメント | 概要 |
|----|--------|----------|--------------|------|
| F-14 | P3 | 未着手 | [features/post-stability-containerization.md](features/post-stability-containerization.md) | コンテナ化・環境変数・musl |
| F-15 | P3 | 未着手 | [features/post-stability-aarch64.md](features/post-stability-aarch64.md) | aarch64 対応 |
| F-16 | P3 | 未着手 | [features/freebsd-support.md](features/freebsd-support.md) | FreeBSD 対応 (kqueue, kTLS, Capsicum 等) |
| F-17 | P3 | 未着手 | [features/openbsd-support.md](features/openbsd-support.md) | OpenBSD 対応 (pledge, unveil 等) |

---

## バグチケット一覧

（チケット発行時に行を追加する）

| ID | 優先度 | 対応状況 | ドキュメント | 概要 |
|----|--------|----------|--------------|------|
| B-01 | P1 | 完了 | [bugs/B-01-iouring-accept-nonblock.md](bugs/B-01-iouring-accept-nonblock.md) | io_uring accept が O_NONBLOCK を設定せず body timeout が発火しない |
| B-02 | P1 | 完了 | [bugs/B-02-408-connection-not-closed.md](bugs/B-02-408-connection-not-closed.md) | 408 送信後も接続を閉じず、クライアントが read タイムアウトまでブロック |
| B-03 | P1 | 完了 | [bugs/B-03-header-size-check-includes-body.md](bugs/B-03-header-size-check-includes-body.md) | ヘッダーサイズチェックにボディバイトが含まれ、正常リクエストが 431 で誤拒否される |
| B-04 | P1 | 完了 | [bugs/B-04-wasm-filter-missing-https-path.md](bugs/B-04-wasm-filter-missing-https-path.md) | WASM レスポンスフィルタが HTTPS バックエンドパスに未適用 |
| B-05 | P1 | 完了 | [bugs/B-05-wasm-modules-thread-local-race.md](bugs/B-05-wasm-modules-thread-local-race.md) | WASM モジュールリストの thread_local 競合により並行リクエストでフィルタが未適用になる |
| B-06 | P2 | 完了 | [bugs/B-06-grpc-h2c-trailer-not-forwarded.md](bugs/B-06-grpc-h2c-trailer-not-forwarded.md) | gRPC H2C レスポンストレーラーが HTTP/1.1 クライアントに転送されない |

---

## メタ

- 実装・仕様変更時は [AGENTS.md](../../AGENTS.md) と README の更新を同じ変更単位で行う。
- AI が生成する作業ログ・レポートは [AGENTS.md](../../AGENTS.md) の **「AI 成果物・ログ・一時ファイル」** に従い **`docs/artifacts/`** に置く（本バックログの個別 md は **仕様・チケット用**）。
