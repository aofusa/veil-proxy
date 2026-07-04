# バックログ: ファジング・カオスエンジニアリング・セキュリティスキャン

## 目的

プロトコルパーサ・バッファ処理・TLS 境界の **堅牢性** と、本番に近い障害下での **生存性** を継続的に検証する。

## スコープ案

### 1. プロトコル相互運用・準拠

| ツール | 狙い |
|--------|------|
| **h2spec** | HTTP/2 実装の準拠・エッジケース |
| **QUIC interop**（quiche エコシステム等） | HTTP/3 / QUIC の相互運用 |

- CI で毎回は重いため、**nightly または週次**ジョブ案。

### 2. ファジング（LibAFL 等）

- 対象候補: HTTP/1 パース、HPACK、カスタムバッファ操作、設定パーサ。
- **libFuzzer / cargo-fuzz** から段階的に LibAFL 等へ拡張する道筋を記載。
- クラッシュ再現用の **最小シードコーパス** をリポジトリまたは別アーティファクトストアに。

### 3. カオスエンジニアリング（Chaos Mesh 等）

- Kubernetes 上で Veil を動かす前提が後続（[post-stability-containerization.md](post-stability-containerization.md)）になる場合が多い。
- シナリオ例: upstream 遅延・断続的ドロップ、DNS 失敗、ノード CPU 圧迫。
- 非 K8s 環境では **tc / iptables** スクリプトによる簡易代替も文書化。

### 4. セキュリティスキャン

- **cargo audit** / **deny**（依存脆弱性）。
- **SAST**（任意の商用 / OSS）。
- **コンテナスキャン**（イメージ化後）。

## 受け入れ条件（案）

- 少なくとも 1 系の **自動ファズ** または **h2spec** を CI に組み込むか、手順が `docs/` に固定されている。
- 重大度の脆弱性に対する対応フロー（SLA なしでもチェックリスト）を README または SECURITY に。

## リスク

- CI 時間・フレーク。ファジングは並列ワーカーとタイムボックスが必須。

---

## 将来拡張（F-07 残件）

[post-stability-containerization.md](post-stability-containerization.md)（F-14）で `docker/` 基盤が整備済み。以下はコンテナ運用（docker-compose / Kubernetes）を見据えた追加機能として backlog に残す。

### 1. 設定のランタイム上書き

| 手段 | 想定用途 |
|------|----------|
| **環境変数** | `VEIL_SERVER_LISTEN` 等、機密を含まないキーのオーバーライド。既存 TOML（serde）との優先順位を定義する。 |
| **CLI 引数** | `-c` 以外に `--set key=value` 形式で起動時のみ上書き。検証（`-t`）経路にも反映する。 |

- 受け入れ条件案: README / docker README に env・CLI の対応表。E2E（コンテナテスト）で代表キーが上書きされること。

### 2. 証明書・秘密鍵の安全な受け渡し

| 手段 | 想定用途 |
|------|----------|
| **ファイルマウント（read-only）** | 現行 `docker/assets/ssl/` + `-v ...:ro`（開発・単一ノード）。 |
| **Docker secrets / K8s Secret** | ボリュームマウント `/run/secrets/tls-cert` 等。パスは env で `config.toml` の `cert_path` / `key_path` と整合。 |
| **環境変数（PEM 本文）** | `VEIL_TLS_CERT_PEM` / `VEIL_TLS_KEY_PEM`（またはファイルパス）。起動時に tmpfs へ書き込まずメモリ上のみで保持する設計を検討。 |
| **SIGHUP リロード** | シークレット差し替え後のゼロダウンタイム更新（F-03 連携）。 |

- 受け入れ条件案: compose / K8s マニフェストのサンプル（参考実装）と、コンテナセキュリティテストで TLS ハンドシェイクが成立すること。

---

## 実施記録

### 2026-07-02（ブランチ `feat/docker`）

`tools/container_security/` を追加し、`veil:glibc` コンテナに対するファジング・カオス・セキュリティ検証を **docker コマンドのみ** で実施した。`tests/e2e_setup.sh` は未使用・未改変。

#### 実行方法

```bash
./tools/container_security/run.sh
```

- ハーネスイメージ `veil-sec-harness:local` をビルド
- Docker ネットワーク `veil-sec-test-net` 上に `veil:glibc` を起動（read-only rootfs、seccomp、TLS/設定マウント）
- ハーネスコンテナ内で各フェーズを実行（Veil へのトラフィックはコンテナ間通信のみ）

#### 結果サマリ

| フェーズ | 内容 | 結果 |
|--------|------|------|
| **ファジング** | HTTP/1.1 シード 14 種 + 変異 200 回（不正ヘッダ・巨大 Host・不完全ボディ等） | `sent=200 transport_errors=0 healthy_after_fuzz=True` — クラッシュなし |
| **カオス** | 並行 HTTP 500 リクエスト×10 ワーカー、HTTPS 100 リクエスト、接続チャーン 100、POST ストレス 50、負荷中 **SIGHUP** リロード | 全 HTTP ワーカー `ok=50 fail=0`、`method_stress denied=49`、SIGHUP 後もヘルス OK |
| **セキュリティ（アプリ）** | TLS ハンドシェイク、POST/TRACE メソッド拒否（HTTPS）、パストラバーサル | TLS OK、POST/TRACE → 405、パストラバーサル OK |
| **セキュリティ（コンテナ）** | read-only rootfs、seccomp 適用、Veil 特権降下ログ | `readonly_rootfs=true`、seccomp 適用済み、`Security restrictions applied` を確認 |
| **Trivy** | `docker save` + `--input` で `veil:glibc` スキャン（HIGH/CRITICAL） | debian 13.5 ベース層 **脆弱性 0 件** |

### 2026-07-02 追記: h2spec 統合

[h2spec](https://github.com/summerwind/h2spec) v2.6.0 を `tools/container_security/harness/` に同梱し、HTTP/2 準拠テストを追加した。

#### 実行方法

```bash
# 全フェーズ（h2spec ゲート含む。デフォルトは generic+hpack のみ）
./tools/container_security/run.sh

# h2spec のみ（開発用）
./tools/container_security/run_h2spec.sh

# RFC 7540/7541 全件（各 ~3.5 分、TLS + H2C で計 ~7 分）
H2SPEC_FULL=1 ./tools/container_security/run_h2spec.sh

# 全件で 1 件でも失敗したら exit 1
H2SPEC_FULL=1 H2SPEC_STRICT=1 ./tools/container_security/run_h2spec.sh
```

| 環境変数 | 既定 | 説明 |
|----------|------|------|
| `SKIP_H2SPEC` | `0` | `1` で h2spec をスキップ |
| `H2SPEC_FULL` | `0` | `1` で RFC 全件スイートを追加実行 |
| `H2SPEC_STRICT` | `0` | `1` でフルスイート失敗時に exit 1 |
| `H2SPEC_TIMEOUT` | `30` | h2spec タイムアウト（秒） |

テスト用設定 `tools/container_security/fixtures/veil-config.toml` で **H2C `:8443`** を有効化し、h2spec 要件（GET/POST `/` → 200 + 非空ボディ）を満たす。

#### h2spec 結果（`veil:glibc`、2026-07-02）

| スイート | 対象 | 結果 |
|--------|------|------|
| **ゲート** `generic hpack` | TLS `:443` | **52/52 通過**（~2s） |
| **ゲート** `generic hpack` | H2C `:8443` | **52/52 通過**（~0.7s） |
| **フル** RFC 7540/7541 | TLS `:443` | 139/146 通過、**7 失敗**（~216s） |
| **フル** RFC 7540/7541 | H2C `:8443` | 138/146 通過、**8 失敗**（~211s） |

フルスイートの主な失敗箇所（TLS/H2C 共通傾向）:

- `http2/5.1` half-closed (remote) ストリームへの DATA/HEADERS/CONTINUATION → `STREAM_CLOSED` 期待
- `http2/6.10` END_HEADERS 後の不正 CONTINUATION → `PROTOCOL_ERROR` 期待

ゲート（相互運用必須セット）は **TLS・H2C とも全通過**。フルスイートの残件は HTTP/2 ストリーム状態機械の厳密準拠として backlog 化を検討。

#### 成果物

- オーケストレータ: `tools/container_security/run.sh`
- h2spec 単体: `tools/container_security/run_h2spec.sh`
- ハーネス: `tools/container_security/harness/`（Dockerfile、h2spec バイナリ、fuzz/chaos/security/h2spec スクリプト）
- テスト設定: `tools/container_security/fixtures/veil-config.toml`
- レポート出力: `tools/container_security/results/`（`h2spec_report.txt`、`h2spec_*_junit.xml` 等）

#### 既知の制限

- HTTP:80 は HTTPS へ 301 リダイレクトするため、メソッド制限検証は HTTPS + DELETE で実施（POST は h2spec 用に許可）
- distroless イメージは Docker `User` 未指定（起動 UID=0）だが、Veil プロセス内で特権降下
- Trivy DB 初回ダウンロードで数分かかる（2 回目以降はキャッシュ）
- h2spec フルスイートは RFC 厳密準拠で 7〜8 件失敗（上記）。既定 CI ではゲートのみ実行
- LibAFL / cargo-fuzz は F-52 で着手（`src/lib.rs` + `fuzz/`）。コンテナ実行は `SKIP_LIBFUZZER=0` で有効化

### 2026-07-02 追記: スイート拡充（F-52〜F-57）

バックログに子チケットを追加し、`tools/container_security/` をプロダクション向けに拡張した。

#### バックログ子チケット

| ID | 概要 | 状態 |
|----|------|------|
| [F-52](F-52-cargo-fuzz-libfuzzer.md) | cargo-fuzz（HPACK・frame・header・config） | 進行中 |
| [F-53](F-53-chaos-engineering-expansion.md) | Toxiproxy + CB + slowloris + reset | 進行中 |
| [F-54](F-54-security-scan-expansion.md) | testssl・cargo-deny・SECURITY.md | 進行中 |
| [F-55](F-55-harness-hardening.md) | metrics リロード検知・レポート集約 | 進行中 |
| [F-56](F-56-property-load-tests.md) | proptest、wrk/k6 | 未着手 |
| [F-57](F-57-container-security-ci.md) | GitHub Actions nightly | 未着手 |

#### 追加ディレクトリ構成

```
tools/container_security/
├── chaos/           # Toxiproxy セットアップ
├── fuzz/            # libFuzzer ラッパー
├── security/        # cargo-audit
├── lib/             # capabilities.sh（io_uring/kTLS ガード）
└── harness/scripts/ # toxiproxy_chaos.sh 等
```

#### 新フェーズ・環境変数

| フェーズ | 内容 | スキップ |
|--------|------|----------|
| 1b libFuzzer | 4 ターゲット（nightly コンテナ） | `SKIP_LIBFUZZER=1`（既定） |
| 3b Toxiproxy | 遅延・reset・CB・slowloris | `SKIP_TOXIPROXY=1` |
| 4a testssl | `run_testssl.sh`（Docker） | `SKIP_TESTSSL=1` |
| 4b cargo-audit | Rust 依存脆弱性 | `SKIP_CARGO_AUDIT=1`（既定） |
| 4c cargo-deny | ライセンス・advisory | `SKIP_CARGO_DENY=1`（既定） |
| 6 レポート | JSON/JUnit サマリ | — |

#### ハーネス堅牢化（F-55）

- SIGHUP 後: 固定 `sleep` → ヘルスエンドポイントのポーリング（`RELOAD_POLL_ATTEMPTS`）
- カオス負荷: バックグラウンドジョブに `wait_with_timeout`（`CHAOS_TIMEOUT_SEC`）
- CI 非対応カーネル: `lib/capabilities.sh` で io_uring/kTLS を検出し正当な理由付きスキップ

#### lib クレート化（F-52）

- `src/lib.rs` + `src/entry.rs`（`veil::run()`）でバイナリとファズターゲットを共有
- `fuzz/fuzz_targets/hpack_decode.rs` — 自作 HPACK デコーダ
- `fuzz/fuzz_targets/config_toml.rs` — `test_config_file` 経由の TOML 検証

#### 実行例

```bash
# 通常（libFuzzer スキップ、h2spec ゲート含む）
./tools/container_security/run.sh

# libFuzzer + Toxiproxy 含むフル拡張
SKIP_LIBFUZZER=0 ./tools/container_security/run.sh
```
