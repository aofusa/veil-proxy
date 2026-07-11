# パフォーマンス計測ハーネス (tools/perf) の実装検証レポート

## 1. 調査目的
`tools/perf` のパフォーマンス計測ハーネスが、以下の要件を満たしているかを検証する。
1. `full features` と `default features` ビルド時の機能が計測対象として網羅されているか。
2. `wasm` 拡張機能（Proxy-Wasm）を有効化したときのパフォーマンス測定が行われているか。

## 2. 現状の実装状況分析

### 2.1 Default Features の網羅性
`Cargo.toml` によると `default = ["ktls", "http2", "mimalloc"]` と定義されている。
`tools/perf/gen_configs.sh` では以下の4因子を完全直交（2^4=16通り）で組み合わせたテスト構成を生成している。
- `http2` (ON/OFF)
- `ktls` (ON/OFF)
- `reuseport_balancing` (cbpf/kernel)
- `open_file_cache` (ON/OFF)

**評価**: Default features（`http2`, `ktls`）のパフォーマンスに対する影響は、直交表によって十分に網羅・測定されている。

### 2.2 Full Features の網羅性
`Cargo.toml` における `full` feature には以下の全機能が含まれている。
- `ktls`: kTLS（Kernel TLS）サポート
- `http2`: HTTP/2 サポート
- `http3`: HTTP/3 (QUIC) サポート
- `grpc-full`: gRPC / gRPC-Web サポート
- `wasm`: WASM Extension (Proxy-Wasm) サポート
- `opentelemetry`: OpenTelemetry (OTLP/HTTP) メトリクスエクスポート
- `compression`: レスポンス圧縮
- `cache`: プロキシキャッシュ
- `metrics`: Prometheusメトリクス出力
- `websocket`: WebSocket プロキシサポート
- `rate-limit`: レートリミット・接続制限
- `buffering`: レスポンスバッファリング制御
- `mimalloc`: 高速メモリアロケータ（ビルド時有効）
- `admin`: 管理 Admin API
- `access-log`: 構造化アクセスログ
- `l4-proxy`: L4 (TCP/UDP) ストリームプロキシ

現状の `gen_configs.sh` では、以下の機能のみがテスト構成として生成・計測されている。
- `ktls` (直交表のベースとして計測)
- `http2` (直交表のベースとして計測)
- `compression` (レスポンス圧縮: zstd/br/gzip)
- `cache` (インメモリキャッシュ)
- `buffering` (高度なバッファリング制御)
- （※ `proxy` 機能も構成に含まれるが、これは明示的な feature フラグではなく基本ルーティング機能である）

**評価**: 上記以外の多数の機能がパフォーマンス計測の対象から漏れている。具体的に **不足している計測対象** は以下の通りである。
- `http3`: QUIC通信時のオーバーヘッドおよびスループット
- `grpc-full`: gRPC/gRPC-Web通信時のフレーミングやトレーラー処理のオーバーヘッド
- `wasm`: Wasmtimeランタイム呼び出し・コンテキストスイッチのオーバーヘッド
- `opentelemetry`: OTLPデータ生成・送信に伴うスレッド間通信およびI/Oのオーバーヘッド
- `metrics`: Prometheusメトリクス（アトミックなカウンタ更新）のオーバーヘッド
- `websocket`: WebSocketハンドシェイク・ストリーミング処理のオーバーヘッド
- `rate-limit`: スライディングウィンドウによるロック競合および演算のオーバーヘッド
- `admin`: Admin API 有効化時のルーティング判定オーバーヘッド
- `access-log`: 構造化ログ（JSON等）のフォーマット処理とファイル出力のオーバーヘッド
- `l4-proxy`: L4 ストリームプロキシ時のレイテンシとスループット

### 2.3 WASM 拡張機能の網羅性
`gen_configs.sh` および `run_perf.sh` の実装を確認したところ、**WASM（Proxy-Wasm）拡張機能のパフォーマンス計測は一切行われていない。**
WASMランタイム（wasmtime）の呼び出しやコンテキストスイッチはデータプレーンのホットパスにおいて大きなオーバーヘッドになる可能性があるが、現状ではその影響を計測する仕組みが欠如している。

---

## 3. 改善内容の提案

WASM拡張機能をはじめとする `full features` 向けのパフォーマンス計測を補完するため、以下の改善を提案する。

### 3.1 WASM 計測用構成の追加
`tools/perf/gen_configs.sh` に WASM 機能を有効化した設定ファイル (`h2_1_feat_wasm.toml`) の生成を追加する。

```bash
# wasm: Proxy-Wasm 拡張（単純なパススルーフィルタでのオーバーヘッド計測）
{
    feat_base_head
    cat <<'EOF'

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/var/www/"
[[route.wasm]]
enabled = true
module_path = "/etc/veil/wasm/dummy_filter.wasm"
[route.security]
allowed_methods = ["HEAD", "GET"]
EOF
} > "$OUT/h2_1_feat_wasm.toml"
echo "wrote $OUT/h2_1_feat_wasm.toml"
count=$((count + 1))
```

### 3.2 テスト用 Dummy WASM モジュールの用意
WASMランタイム自体のベースライン・オーバーヘッドを純粋に計測するため、リクエスト/レスポンスを何も変更せずにパススルーするだけの非常に軽量な `dummy_filter.wasm` を `docker/assets/wasm/` 等に用意する。

### 3.3 コンテナ実行時の Volume マウント追加
`tools/perf/run_perf.sh` の `start_veil` 関数にて、WASMモジュールが格納されているディレクトリを読み取り専用でマウントするよう修正する。

```bash
    docker run -d --rm --network $NET \
        --read-only \
        --tmpfs /var/cache/veil:rw,noexec,nosuid,uid=65532,gid=65532,size=512m \
        --tmpfs /var/tmp/veil:rw,noexec,nosuid,uid=65532,gid=65532,size=256m \
        -v "$mount_cfg:/etc/veil/conf.d/config.toml:ro" \
        -v "$ASSETS/ssl:/etc/veil/ssl:ro" \
        -v "$ASSETS/www:/var/www:ro" \
        -v "$ASSETS/wasm:/etc/veil/wasm:ro" \  # ← 追加
        --security-opt seccomp="$ASSETS/security/seccomp.json" \
        --name "$name" "$img" >/dev/null
```

### 3.4 不足している全機能に対するパフォーマンス計測の追加提案
WASM以外に不足している以下の各機能についても、ベース構成 (`h2_1_feat_xxx.toml`) に対して1機能だけを有効化したバリアントを `gen_configs.sh` に追加し、パフォーマンス影響（オーバーヘッド）を定量的に可視化することを提案する。

1. **`http3` (HTTP/3, QUIC)**
   - `http3_enabled = true` とし、UDP/QUIC のスループットとCPU負荷を計測する（計測ツールに h3load や quiche のベンチマーククライアント等を導入する必要あり）。
2. **`grpc-full` (gRPC / gRPC-Web)**
   - gRPC ルーティング（`type = "Grpc"` 等）を定義し、ghz などの gRPC ベンチマークツールを用いてレイテンシを計測する。
3. **`opentelemetry` (OTLP/HTTP)**
   - OTLP エクスポートを有効化し、テレメトリデータ生成・送信によるデータプレーンスレッドへの干渉・CPUオーバーヘッドを計測する。
4. **`metrics` (Prometheus)**
   - `[metrics]` を有効化し、リクエストごとのアトミック変数更新が極限負荷環境でキャッシュライン競合（False Sharing 等）を引き起こさないかを計測する。
5. **`websocket` (WebSocket)**
   - WebSocket の Echo バックエンドを用意し、通常の HTTP プロキシと比較した時のハンドシェイクおよびフレーム転送のオーバーヘッドを計測する。
6. **`rate-limit`**
   - リクエスト上限に達しない範囲でのレートリミットを有効化し、状態管理（スライディングウィンドウ等）のロック競合や演算によるレイテンシ低下を計測する。
7. **`admin`**
   - Admin API を有効化し、通常のリクエストのルーティング判定パスにおいてオーバーヘッドが増加していないかを計測する。
8. **`access-log`**
   - JSON などの構造化アクセスログ出力を有効化し、ログバッファリングやディスクI/Oがホットパスのレイテンシ (p99等) に与える影響を計測する。
9. **`l4-proxy`**
   - L4 (TCP/UDP) ストリームプロキシのルートを構成し、L7 処理をバイパスした純粋なパケット転送のスループットとカーネル/ユーザ空間のコンテキストスイッチコストを計測する。
