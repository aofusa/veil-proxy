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
| F-02 | P1 | 進行中 | [features/e2e-test-hardening.md](features/e2e-test-hardening.md) | E2E の網羅・実装乖離の解消 |
| F-03 | P1 | 未着手 | [features/tls-cert-zero-downtime.md](features/tls-cert-zero-downtime.md) | 0 ダウンタイム TLS 証明書更新 |
| F-04 | P1 | 未着手 | [features/vds-xds-dynamic-config.md](features/vds-xds-dynamic-config.md) | 動的設定配信 API（VDS / xDS 相当） |
| F-06 | P1 | 未着手 | [features/resilience-outlier-detection.md](features/resilience-outlier-detection.md) | サーキットブレーカー・リトライ・異常検知 |
| F-09 | P1 | 未着手 | [features/prometheus-feature-flags.md](features/prometheus-feature-flags.md) | Prometheus 拡充と feature 無効化 |
| F-01 | P2 | 完了 | [features/grpc.md](features/grpc.md) | gRPC / gRPC-Web の完成度・テスト拡充 |
| F-05 | P2 | 未着手 | [features/acme.md](features/acme.md) | ACME 統合 |
| F-07 | P2 | 未着手 | [features/fuzzing-chaos-security.md](features/fuzzing-chaos-security.md) | ファジング・カオス・セキュリティスキャン |
| F-08 | P2 | 未着手 | [features/proxy-wasm-benchmarks.md](features/proxy-wasm-benchmarks.md) | Proxy-Wasm ベンチマーク |
| F-10 | P2 | 未着手 | [features/opentelemetry.md](features/opentelemetry.md) | OpenTelemetry 対応 |
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
| — | — | — | — | *未登録* |

---

## メタ

- 実装・仕様変更時は [AGENTS.md](../../AGENTS.md) と README の更新を同じ変更単位で行う。
- AI が生成する作業ログ・レポートは [AGENTS.md](../../AGENTS.md) の **「AI 成果物・ログ・一時ファイル」** に従い **`docs/artifacts/`** に置く（本バックログの個別 md は **仕様・チケット用**）。
