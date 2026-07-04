# container_security テストスイート

Veil の **Docker イメージ**（`veil:glibc` 等）を対象に、ファジング・カオスエンジニアリング・プロトコル準拠・セキュリティスキャンを **docker コマンドのみ** で実行するオーケストレータです。

ホストに Rust や h2spec をインストールする必要はありません。バックログ上は [F-07](../../docs/backlog/features/fuzzing-chaos-security.md) の一部で、F-52〜F-57 で段階的に拡充されています。

---

## 前提条件

| 項目 | 内容 |
|------|------|
| Docker | daemon が起動し、`docker info` が成功すること |
| Veil イメージ | 既定 `veil:glibc`（[docker/README.md](../../docker/README.md) 参照） |
| TLS 証明書 | `docker/assets/ssl/cert.pem` と `key.pem` が存在すること |
| seccomp | `docker/assets/security/seccomp.json`（io_uring 許可） |

イメージのビルド例:

```bash
cd docker
docker build -f Dockerfile.glibc -t veil:glibc --build-arg CARGO_FEATURES='full' ..
```

証明書が無い場合:

```bash
openssl req -x509 -newkey EC -pkeyopt ec_paramgen_curve:secp384r1 -nodes \
    -keyout docker/assets/ssl/key.pem \
    -out docker/assets/ssl/cert.pem \
    -days 3650 \
    -subj "/CN=localhost" \
    -addext "subjectAltName=DNS:localhost,IP:127.0.0.1"
```

---

## クイックスタート

リポジトリルートから:

```bash
# 既定（libFuzzer / cargo-audit / cargo-deny はスキップ）
./tools/container_security/run.sh
```

所要時間の目安: 10〜20 分（h2spec・Toxiproxy カオス含む）

レポート出力先: `tools/container_security/results/`

---

## 実行パターン

### 通常 CI / 開発

```bash
./tools/container_security/run.sh
```

### スモーク（重いフェーズを省略）

```bash
SKIP_TRIVY=1 SKIP_H2SPEC=1 SKIP_TESTSSL=1 ./tools/container_security/run.sh
```

### nightly / リリース前（フル拡張）

```bash
SKIP_LIBFUZZER=0 SKIP_CARGO_AUDIT=0 SKIP_CARGO_DENY=0 SKIP_TRIVY=0 \
  ./tools/container_security/run.sh
```

### libFuzzer のみ

```bash
# 既定: 各ターゲット 2000 runs、最大 120 秒/ターゲット
./tools/container_security/fuzz/run_libfuzzer.sh

# runs / 時間を変更
FUZZ_RUNS=5000 FUZZ_MAX_TIME=300 ./tools/container_security/fuzz/run_libfuzzer.sh
```

初回はコンテナ内で `cargo-fuzz` のインストールと `veil` のビルドが走るため **15〜20 分** かかることがあります。2 回目以降はキャッシュで短縮されます。

検証済み例（4 ターゲット × 2000 runs、クラッシュなし、`libfuzzer: ok`）:

```bash
FUZZ_RUNS=2000 FUZZ_MAX_TIME=120 ./tools/container_security/fuzz/run_libfuzzer.sh
```

### h2spec のみ

```bash
./tools/container_security/run_h2spec.sh
```

---

## テストフェーズ一覧

`run.sh` は以下の順で実行します。終了時に `trap cleanup` でコンテナ・ネットワークを削除します。

| フェーズ | スクリプト / ツール | 検証内容 |
|----------|---------------------|----------|
| 0 起動 | `lib/common.sh` | ネットワーク作成、ハーネスビルド、Veil 起動、ヘルス待ち |
| 0b カオス基盤 | `chaos/toxiproxy_setup.sh` | whoami backend + Toxiproxy 2.9（API :8474、proxy :8480） |
| 0c 実行時セキュリティ | `validate_veil_image_security` | ReadonlyRootfs、seccomp、特権降下ログ |
| 1 HTTP ファジング | `harness/scripts/fuzz_http.py` | 不正ヘッダー・不完全リクエストのブラックボックス送信 |
| 1b libFuzzer | `fuzz/run_libfuzzer.sh` | HPACK / TOML / HTTP/2 フレーム / HTTP/1 ヘッダー境界（オプション） |
| 1c libFuzzer(ASAN) | `fuzz/run_libfuzzer_asan.sh` | 上記を AddressSanitizer + 永続 corpus で実行（F-71、既定 SKIP） |
| 2 h2spec | `harness/scripts/h2spec_run.sh` | HTTP/2 プロトコル準拠（TLS + H2C） |
| 3 カオス負荷 | `harness/scripts/chaos_load.sh` | 高並行 HTTP/HTTPS、接続チャーン、SIGHUP リロード |
| 3b Toxiproxy | `toxiproxy_chaos.sh` | 遅延注入・回復、`limit_data` による接続リセット |
| 3c サーキットブレーカー | `circuit_breaker_chaos.sh` | upstream 障害 → CB 発火 → 回復（Prometheus + 5xx カウント） |
| 3d slowloris | `slowloris_chaos.sh` | 部分リクエストによる backpressure、事後ヘルス |
| 3e bad_backend | `chaos/bad_backend_chaos.sh` | バックエンドのプロトコル違反（F-67、既定 SKIP。B-16/B-17 検出実績） |
| 3f Pumba netem | `chaos/pumba_chaos.sh` | パケットロス/遅延/重複/破損の注入と回復（F-69、既定 SKIP） |
| 3g リソース枯渇 | `chaos/resource_exhaustion_chaos.sh` | cgroup 制約下の高並行負荷で panic/OOM 回避（F-68、既定 SKIP） |
| 4 セキュリティ | `security_scan.sh` | TLS ハンドシェイク、メソッド制限、TRACE、パストラバーサル |
| 4a testssl | `security/run_testssl.sh` | `drwetter/testssl.sh` コンテナで TLS 設定スキャン |
| 4b cargo-audit | `security/run_cargo_audit.sh` | Rust 依存関係の脆弱性 |
| 4c cargo-deny | `security/run_cargo_deny.sh` | advisory + ライセンス（`deny.toml`） |
| 4d semgrep(SAST) | `security/run_semgrep.sh` | 自作コードの静的解析（`p/rust`+`p/security-audit`、F-64） |
| 4e SBOM | `security/run_sbom.sh` | syft で source(CycloneDX)+image(SPDX) 生成（F-65） |
| 4f ZAP(DAST) | `security/run_zap.sh` | OWASP ZAP baseline（F-66、既定 SKIP） |
| 5 Trivy | `run_trivy_scan` | イメージ脆弱性（HIGH/CRITICAL） |
| 6 レポート集約 | `lib/report.sh` | `suite_summary.json` / `suite_summary_junit.xml` |
| 最終 | `health_check.sh` | 全フェーズ後の応答確認（最大 10 回リトライ） |

---

## libFuzzer ターゲット（F-52）

`fuzz/` クレート（`veil-fuzz`）から `veil` ライブラリをリンクして実行します。

| ターゲット | 対象 |
|-----------|------|
| `hpack_decode` | HTTP/2 HPACK デコーダ |
| `config_toml` | 設定 TOML パーサ（`test_config_file`） |
| `http2_frame_decode` | HTTP/2 フレームデコーダ |
| `http_header_validate` | HTTP/1 ヘッダー名・値の境界検証（`fuzz_api`） |
| `wasm_abi` | WASM モジュール/ABI 境界（`fuzz_api::wasm_module_smoke`、F-70。`--features wasm` で有効化） |

環境変数:

| 変数 | 既定 | 説明 |
|------|------|------|
| `FUZZ_RUNS` | `2000` | ターゲットあたりの実行回数 |
| `FUZZ_MAX_TIME` | `120` | ターゲットあたりの最大秒数 |
| `FUZZ_TARGETS` | 上記 4 つ | スペース区切りでターゲット指定 |
| `RUST_FUZZ_IMAGE` | `rustlang/rust:nightly-bookworm` | ビルド用 Rust イメージ |
| `SKIP_LIBFUZZER` | `1`（`run.sh` 経由時） | `0` で libFuzzer フェーズを有効化 |

ローカル（cargo-fuzz インストール済み）での短時間確認:

```bash
CARGO_TARGET_DIR=/tmp/veil-build-target cargo build -p veil-fuzz
/tmp/veil-build-target/debug/hpack_decode -runs=1000
```

コンテナ実行の成功条件: レポート末尾に `libfuzzer: ok` が出力されること。

---

## 環境変数（オーケストレータ）

### スキップ制御

| 変数 | 既定 | 説明 |
|------|------|------|
| `SKIP_LIBFUZZER` | `1` | libFuzzer フェーズ |
| `SKIP_H2SPEC` | `0` | h2spec |
| `SKIP_TOXIPROXY` | `0` | Toxiproxy 系カオス（3b〜3d） |
| `SKIP_TRIVY` | `0` | Trivy イメージスキャン |
| `SKIP_TESTSSL` | `0` | testssl.sh |
| `SKIP_CARGO_AUDIT` | `1` | cargo-audit |
| `SKIP_CARGO_DENY` | `1` | cargo-deny |
| `SKIP_SEMGREP` | `0` | semgrep（SAST、F-64） |
| `SKIP_SBOM` | `0` | SBOM 生成（syft、F-65） |
| `SKIP_ZAP` | `1` | OWASP ZAP baseline（DAST、F-66） |
| `SKIP_BAD_BACKEND` | `1` | バックエンドプロトコル違反カオス（F-67） |
| `SKIP_PUMBA` | `1` | Pumba netem カオス（F-69） |
| `SKIP_RESOURCE_EXHAUSTION` | `1` | リソース枯渇カオス（F-68） |
| `SKIP_LIBFUZZER_ASAN` | `1` | libFuzzer + ASAN（F-71） |
| `SKIP_CHAOS_LOAD` | `0` | 高並行負荷 + SIGHUP（`KERNEL_REQUIRE_IO_URING=1` 時は自動） |

### h2spec

| 変数 | 既定 | 説明 |
|------|------|------|
| `H2SPEC_FULL` | `0` | `1` でフルスイート（時間増） |
| `H2SPEC_STRICT` | `0` | 厳密モード |
| `H2SPEC_TIMEOUT` | `30` | タイムアウト秒（ハーネスへ渡す） |

### タイムアウト・ポーリング

| 変数 | 既定 | 説明 |
|------|------|------|
| `CHAOS_TIMEOUT_SEC` | `300` | カオス負荷バックグラウンドジョブの上限 |
| `RELOAD_POLL_ATTEMPTS` | `30` | SIGHUP 後のメトリクス+ヘルスポーリング回数 |
| `METRICS_PATH` | `/__metrics` | リロード完了検知用 Prometheus パス |

### イメージ・リソース名

| 変数 | 既定 |
|------|------|
| `VEIL_IMAGE` | `veil:glibc` |
| `HARNESS_IMAGE` | `veil-sec-harness:local` |
| `NET_NAME` | `veil-sec-test-net` |
| `RESULTS_DIR` | `tools/container_security/results` |
| `TRIVY_SEVERITY` | `HIGH,CRITICAL` |

### カーネル能力

| 変数 | 説明 |
|------|------|
| `KERNEL_REQUIRE_IO_URING=1` | io_uring 非対応ホストで chaos 負荷をスキップ（理由をログ・レポートに記録） |

---

## ディレクトリ構成

```
tools/container_security/
├── run.sh                 # メインオーケストレータ
├── run_h2spec.sh          # h2spec のみ
├── README.md              # 本ファイル
├── fixtures/
│   └── veil-config.toml   # テスト用 Veil 設定（Landlock・CB・prometheus）
├── chaos/
│   └── toxiproxy_setup.sh # backend + Toxiproxy 起動
├── fuzz/
│   └── run_libfuzzer.sh   # cargo-fuzz ラッパー
├── security/
│   ├── run_cargo_audit.sh
│   ├── run_cargo_deny.sh
│   └── run_testssl.sh
├── lib/
│   ├── common.sh          # 起動・cleanup・run_harness
│   ├── capabilities.sh    # io_uring / kTLS 検出
│   └── report.sh          # JSON / JUnit 集約
├── harness/
│   ├── Dockerfile         # debian + curl + h2spec + python3
│   ├── fixtures/seeds/    # HTTP ファジングシード
│   └── scripts/           # 各フェーズの実行スクリプト
└── results/               # レポート出力（gitignore、.gitkeep のみ追跡）
```

---

## テスト用設定の要点

`fixtures/veil-config.toml` はランタイムで `results/veil-config.runtime.toml` にコピーされ、Veil コンテナへマウントされます。

- **Landlock** 有効（seccomp と併用）
- **Prometheus** `/__metrics` 有効（SIGHUP リロード検知・CB メトリクス用）
- **ルート `/`**: 静的ファイル（h2spec 用 200 応答）
- **ルート `/api/`**: `toxi-pool` upstream（Toxiproxy 経由）+ サーキットブレーカー

Toxiproxy 起動後、`prepare_veil_test_config()` が Docker DNS ホスト名 `veil-sec-toxiproxy` を **コンテナ IP に置換** します（Landlock 下の DNS/NSS 制約回避）。

---

## レポート

| ファイル | 内容 |
|----------|------|
| `fuzz_report.txt` | HTTP ブラックボックスファジング |
| `libfuzzer_report.txt` | libFuzzer 各ターゲット |
| `h2spec_*_junit.xml` | h2spec JUnit |
| `chaos_report.txt` | 高並行負荷 |
| `toxiproxy_chaos_report.txt` | 遅延・reset |
| `circuit_breaker_chaos_report.txt` | サーキットブレーカー |
| `slowloris_chaos_report.txt` | slowloris |
| `security_scan_report.txt` | TLS・メソッド制限等 |
| `testssl_report.txt` | testssl.sh |
| `cargo_audit_report.txt` | cargo-audit |
| `cargo_deny_report.txt` | cargo-deny |
| `trivy_report.txt` | イメージ脆弱性 |
| `kernel_capabilities.txt` | io_uring / kTLS |
| `suite_summary.json` | フェーズ別 pass/fail/skip 集約 |
| `suite_summary_junit.xml` | CI 向けサマリ |
| `container_inspect.json` | Veil コンテナ inspect |

HIGH/CRITICAL 脆弱性の対応フローはリポジトリ直下の [SECURITY.md](../../SECURITY.md) を参照してください。

---

## トラブルシューティング

### Veil が起動しない

- `veil:glibc` イメージが存在するか: `docker images veil:glibc`
- TLS 証明書・seccomp プロファイルのパスを確認
- 前回のコンテナが残っている場合:

```bash
docker rm -f veil-sec-proxy veil-sec-harness veil-sec-toxiproxy veil-sec-backend veil-sec-trivy
docker network rm veil-sec-test-net
```

### Toxiproxy / `/api/` が 502

- Landlock 有効時は上流 URL が IP 置換されているか `results/veil-config.runtime.toml` を確認
- `docker logs veil-sec-toxiproxy` で proxy 状態を確認

### SIGHUP カオスで警告が出る

高並行負荷と SIGHUP が重なると、メトリクス/ヘルス復帰のポーリングが間に合わないことがあります。オーケストレータは警告ログのあと継続し、**最終 health チェック** で判定します。

### libFuzzer が遅い / ディスクを消費する

- ビルド成果物: `results/cargo-target/`（`.gitignore` 対象）
- 2 回目以降は Docker volume キャッシュで短縮
- CI では `SKIP_LIBFUZZER=1`（既定）を維持し、nightly のみ `SKIP_LIBFUZZER=0`

### ホストビルドで target/ 権限エラー

```bash
CARGO_TARGET_DIR=/tmp/veil-build-target cargo build -p veil-fuzz
```

---

## 関連ドキュメント

| パス | 内容 |
|------|------|
| [docker/README.md](../../docker/README.md) | イメージビルド・単体コンテナ実行 |
| [SECURITY.md](../../SECURITY.md) | 脆弱性報告・スキャン対応フロー |
| [docs/backlog/features/fuzzing-chaos-security.md](../../docs/backlog/features/fuzzing-chaos-security.md) | バックログ親チケット（F-07） |
| [fuzz/](../../fuzz/) | cargo-fuzz ターゲット定義 |

---

## 残作業（バックログ）

| ID | 残件 |
|----|------|
| F-52 | ASAN/TSAN ビルド、corpus Artifact 保存 |
| F-53 | Pumba / tc パケットロス |
| F-54 | OWASP ZAP / Nuclei、SBOM |
| F-55 | GitHub Actions matrix（glibc/musl） |
| F-57 | CI/nightly への統合 |