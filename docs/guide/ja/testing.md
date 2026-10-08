# テスト（開発者向け）

[← ドキュメント目次](README.md) · [English](../testing.md)

## テスト

Veilには、ユニットテスト、統合テスト、エンドツーエンド（E2E）テストを含む包括的なテストスイートが含まれています。

### テスト概要

| テスト種別 | テスト数 | 状態 |
|-----------|---------|------|
| **ユニットテスト**（lib） | 694 | ✅ すべて成功 |
| **統合テスト**（`integration_tests` + プロパティ + キャンセル安全性） | 55+ | ✅ すべて成功 |
| **E2Eテスト**（`e2e_tests`） | 419 | ✅ すべて成功 |
| **Fuzz ターゲット**（`cargo fuzz`） | 8 | ✅ クラッシュなし |
| **ベンチマーク** | 13ファイル | ✅ 準備完了 |

Fuzz ターゲットには `io_uring_executor` が含まれる。ランタイムの完了ディスパッチ経路
（`src/runtime/executor.rs`）へ任意の擬似 CQE 列を注入し、op テーブルが panic せず・スロットを
リークせず・drop ガードを二重実行しないことを検証する（実リング上で in-flight な
recv/send/accept/timer Future をランダムに Drop する `runtime_cancellation_test` 統合テストと補完関係）。

ホットパス規則（「イベントループ上でブロッキング呼び出しをしない」）は
[`clippy.toml`](../../../clippy.toml) の clippy `disallowed-methods` により AST レベルで強制する。
同期 `std::fs`・`std::thread::sleep`・ブロッキング `std::net` ソケットをデータプレーンのコードで
拒否し、正当な利用箇所（offload 閉包・専用スレッド・起動/リロードのコールドパス・テスト/ベンチ）
には理由付き `#[allow]` を付与する。

ユニットテスト数はリリースイメージビルド（`docker/Dockerfile.musl` が
`cargo test --lib --features full` を実行 = 883 passed）で検証している。
リポジトリ内テストに加え、Docker ベースの外形検証ハーネスを 2 つ用意している。

- **[`tools/container_security/`](../../../tools/container_security/)** — ファジング・カオス・
  h2spec（HTTP/2 準拠）・リクエストスマグリング/差分プローブ・イメージ/セキュリティ
  スキャンを **full features** のコンテナイメージに対して実行する。ファジングと不正
  バックエンドモックは Rust バイナリ（Python 非依存）、TLS/平文プローブは `openssl` /
  bash `/dev/tcp` を用いる。
- **[`tools/perf/`](../../../tools/perf/)** — `veil:glibc` / `veil:musl`（full features）と
  `nginx` のスループット/レイテンシ/CPU/メモリ比較。HTTP/1.1（`wrk`）・HTTP/2（`h2load`）・
  **HTTP/3（QUIC 対応 `h2load`）**・**gRPC / WebSocket（`grafana/k6`）** の各クライアントで計測する。
  default+http2 チューニング（2⁴=16 直交表）に加え、ベースへ 1 機能だけを重ねた機能ショーケース構成
  （compression / cache / buffering / 逆プロキシ /
  **wasm / metrics / access-log / rate-limit / admin / opentelemetry / l4-proxy / http3 / grpc /
  websocket**、`h2_1_feat_*` / `h2_0_feat_l4`）で機能単位のオーバーヘッドを計測する。さらに
  **全プロトコル×全機能マトリクス**（`h2_1_proxy_*` / `h3_file_*` / `h3_proxy*` / `grpc_h2_*` /
  `grpc_h3*`）を生成し、Proxy / HTTP/3 / gRPC(over H2/H3) 経路へ各機能を重ねて計測する
  （`CONFIG_GLOB` で対象を絞り込み可。gRPC over H3 は k6 非対応のためフェイルセーフで `NA`）。HTTP/3 は
  QUIC 対応 h2load（`docker build -t local/h2load-h3:latest tools/perf/h2load-http3`。未ビルド時は
  スキップ）、gRPC/WebSocket は `moul/grpcbin` / `jmalloc/echo-server` を上流に `grafana/k6` で計測する。
  結果のサマリと生データ（TSV）は
  [docs/perf/README.md](../../perf/README.md) / [docs/perf/results_raw.tsv](../../perf/results_raw.tsv) を参照
  （TLS 終端が支配的コストで平文 L4 は最大 2.2 倍、L7 機能ロジックはノイズ範囲内・全構成 Non-2xx=0）。
  最新結果（2026-07-16、v0.5.0 向けフルスイート。全 105 構成×プロトコルで Non-2xx=0、同 README）:
  HTTP/1.1 3213 req/s（nginx 比 1.47 倍）/ HTTP/2 2763 req/s（nginx 比 1.18 倍。
  マルチスレッド h2load では 3646 req/s で **F-116 多重化により HTTP/1.1 超え**）/
  **HTTP/3 835 req/s（F-115 recvmmsg/sendmmsg バッチング + B-43 StreamBlocked 修正で 2 倍化。
  `--net=host` 参考値 ~906 req/s）** / gRPC 中継 1609 req/s
  （k6→grpcbin 直行の対照計測でプロキシホップのオーバーヘッドは実質ゼロ）/
  L4 平文素通し 5074 req/s。
  v0.5.0 フルスイート計測では B-44（起動時 RLIMIT_NOFILE 引き上げ + バックエンド接続
  チャーン）・B-45（L4 半クローズ伝搬）・B-46（HTTP/3 バッファ経路の content-length 重複）も
  検出・修正した。
  なお HTTP/3 の mmsg バッチングは Docker seccomp 許可リストに `recvmmsg`/`sendmmsg` が必要
  （`docker/assets/security/seccomp.json` は対応済み）。

### テストの実行

#### ユニットテスト

```bash
# すべてのユニットテストを実行
cargo test --features full --bin veil

# 特定のテストモジュールを実行
cargo test --features full --bin veil wasm::tests

# 出力付きで実行
cargo test --features full --bin veil -- --nocapture
```

#### 統合テスト

```bash
# 統合テストを実行
cargo test --test integration_tests --features full
```

#### E2Eテスト

E2Eテストは実行中のテスト環境が必要です。セットアップスクリプトを使用してください：

```bash
# 方法1: 自動実行（推奨）
./tests/e2e_setup.sh test

# 方法2: 手動実行
./tests/e2e_setup.sh start
cargo test --test e2e_tests --features full -- --test-threads=1
./tests/e2e_setup.sh stop

# クリーンアップのみ
./tests/e2e_setup.sh clean
```

#### ベンチマーク

```bash
# E2E環境を起動
./tests/e2e_setup.sh start

# すべてのベンチマークを実行
cargo bench --features full

# 特定のベンチマークを実行
cargo bench --bench throughput --features full
cargo bench --bench latency --features full

# WASM フィルタオーバーヘッド（WASM ルート付きでプロキシ起動が前提。e2e_setup を利用）。
# WASM 適用ルート（/wasm/*）と非適用ルート（/）の同一リクエストを比較し、Keep-Alive 群で
# 接続コストを償却して 1 リクエストあたりのフィルタオーバーヘッドを切り出す。
# 期待オーダー: ヘッダフィルタで 1 リクエストあたり数 µs〜数十 µs（マシン/wasmtime 依存）。
# RSS は `/usr/bin/time -v` で別途計測。
cargo bench --bench wasm --features wasm

# 環境を停止
./tests/e2e_setup.sh stop

# または自動化スクリプトを使用
./tests/run_bench.sh              # すべてのベンチマーク
./tests/run_bench.sh throughput   # スループットのみ
./tests/run_bench.sh latency      # レイテンシのみ
```

### テストカバレッジ

#### ユニットテスト (469テスト)

- **CIDR/IPフィルタリング**: IPアドレスフィルタリング、CIDR範囲検証
- **レート制限**: スライディングウィンドウレート制限、エントリ管理
- **設定パース**: TOMLパース、デフォルト値
- **ロードバランシング**: Round Robin、Least Connections、IP Hashアルゴリズム
- **ヘルスチェック**: サーバー状態管理、成功/失敗カウント
- **コネクションプール**: プール管理、タイムアウト検証
- **キャッシュ管理**: メモリ/ディスクキャッシュ、キー生成
- **HTTP/2**: フレームエンコード/デコード、HPACK圧縮
- **セキュリティ**: セキュリティ設定、カーネルバージョン検出
- **WASM**: Proxy-Wasm ABI、フィルターライフサイクル、ホスト関数コールバック
- **ユーティリティ**: 各種ヘルパー関数

#### 統合テスト (12テスト)

- TCP接続処理
- HTTPサーバーレスポンス
- 複数サーバー連携
- 動的ポート割り当て
- TLS証明書生成
- 設定ファイル生成
- ポート可用性ユーティリティ

#### E2Eテスト (24テスト)

- **プロキシコア**: 基本リクエスト、ヘルスエンドポイント
- **ヘッダー操作**: ヘッダーの追加/削除、バックエンドID
- **ロードバランシング**: Round Robin分散
- **静的ファイル配信**: インデックスファイル、大容量ファイル
- **圧縮**: gzip、brotli、優先順位処理
- **バックエンドアクセス**: 直接バックエンド接続
- **Prometheus**: メトリクスエンドポイント
- **エラーハンドリング**: 404レスポンス
- **HTTPリダイレクト**: HTTPからHTTPSへのリダイレクト
- **並行性**: 並行および順次リクエスト
- **パフォーマンス**: レスポンスタイム検証
- **Content-Type**: HTML、JSON処理
- **Keep-Alive**: 持続接続
- **カスタムヘッダー**: User-Agent、Hostヘッダー

### 環境クリーンアップ

すべてのテスト環境は自動的にクリーンアップされます：

- **Rust Dropトレイト**: サーバー構造体がスコープを抜けると自動終了
- **シェルスクリプトのtrap**: 成功/失敗/中断時にクリーンアップ
- **Graceful Shutdown**: SIGTERM → 待機 → SIGKILLの段階的終了
- **プロセスクリーンアップ**: 残存プロセスの自動クリーンアップ

クリーンアップ機構により、テスト結果に関わらず、テスト実行後にクリーンな状態が保証されます。

### テストファイル構造

```
veil-proxy/
├── src/
│   ├── main.rs          # 103ユニットテスト
│   ├── security.rs      # 26ユニットテスト
│   ├── cache/           # 50+ユニットテスト
│   ├── http2/           # 30+ユニットテスト
│   └── ...
├── tests/
│   ├── integration_tests.rs  # 13統合テスト
│   ├── e2e_tests.rs          # 24 E2Eテスト
│   ├── e2e_setup.sh          # E2E環境セットアップ
│   ├── run_bench.sh          # ベンチマーク自動化
│   └── common/
│       └── mod.rs            # テストユーティリティ
└── benches/
    ├── throughput.rs      # スループットベンチマーク
    ├── latency.rs         # レイテンシベンチマーク
    ├── http2.rs           # HTTP/2ベンチマーク
    ├── http3.rs           # HTTP/3ベンチマーク
    ├── tls.rs             # TLSベンチマーク
    ├── compression.rs     # 圧縮ベンチマーク
    ├── connection_pool.rs # コネクションプールベンチマーク
    ├── cache.rs           # キャッシュベンチマーク
    ├── load_balancing.rs  # ロードバランシングベンチマーク
    ├── websocket.rs       # WebSocketベンチマーク
    ├── memory.rs          # メモリ使用量ベンチマーク
    └── routing.rs         # ルーティングベンチマーク
```

### 継続的インテグレーション

CI/CDパイプライン用の例：

```yaml
# GitHub Actionsワークフローの例
- name: テストを実行
  run: |
    cargo test --features http2 --all-targets
    
- name: E2Eテストを実行
  run: |
    ./tests/e2e_setup.sh test
```
