# バックログ: Proxy-Wasm ベンチマークテスト追加

## 目的

`wasm` feature 有効時、**フィルタのオーバーヘッド**を定量化し、回帰を防ぐ。

## 現状

- [benches/](../../../benches/) に各種 Criterion ベンチがある。WASM 経路の **専用ベンチ**は拡充余地あり。
- `examples/wasm-filters/` にサンプルフィルタあり。

## スコープ案

1. **ベンチシナリオ**
   - 同一ルートで「WASM なし / ヘッダ変更のみ / HTTP コールあり」等の比較。
   - ウォームアップ、インスタンスプール（Pooling Allocator）の効果測定。

2. **指標**
   - レイテンシ p50/p99、スループット、WASM 燃料（あれば）、メモリ RSS。

3. **CI 方針**
   - `cargo bench` 全件は重いため、**閾値付きスモーク**（前回比 +X% で警告）または手動ワークフロー。

## 受け入れ条件（案）

- `wasm` feature で `cargo bench --bench <wasm>` が再現手順付きで動く。
- README に「期待オーダー」の目安（例: ヘッダフィルタで +N µs）を記載（環境依存である旨の注記付き）。

## 依存

- E2E 環境とベンチ環境の分離（ノイズ低減）。

## 対応状況: 完了

`benches/wasm.rs` を追加し、`Cargo.toml` に `[[bench]] name = "wasm"` を登録した。

### 実装方針

veil は **バイナリクレート（`src/lib.rs` なし）** のため、内部 API を直接叩くマイクロ
ベンチは作れない。既存ベンチ（`latency` / `http2` / `tls` 等）と同様に、稼働中プロキシへの
**ブラックボックス計測**として実装した。E2E セットアップ（`./tests/e2e_setup.sh start`）は
`/wasm/*` ルートに `header_filter` WASM モジュールを適用するため、これを利用して
「WASM 適用ルート vs 非適用ルート」のレイテンシ差で**フィルタオーバーヘッドを定量化**する。

### 計測シナリオ

| グループ | ケース | 内容 |
|----------|--------|------|
| `wasm_overhead_per_connection` | `no_wasm` / `header_filter` | 新規接続あたり（`GET /` vs `GET /wasm/bench`、`Connection: close`） |
| `wasm_overhead_keepalive` | `no_wasm` / `header_filter` | Keep-Alive で接続コストを償却した 1 リクエストあたり（×50）。差分が WASM 純オーバーヘッドの近似 |

Keep-Alive 版は TLS ハンドシェイク/接続確立コストを償却するため、ヘッダフィルタ単体の
往復コストを最も鋭く観測できる。

### 再現手順

```bash
# WASM フィルタ付きで E2E 環境を起動（proxy は --features wasm でビルドされる）
./tests/e2e_setup.sh start
# ベンチ実行
cargo bench --bench wasm --features wasm
# 環境停止
./tests/e2e_setup.sh stop
```

プロキシ未起動、または `/wasm/*` ルートが無い場合は各ベンチが自動でスキップする。

### 受け入れ条件の充足

- [x] `cargo bench --bench wasm --features wasm` が再現手順付きで動く（プロキシ稼働が前提）。
- [x] README に「期待オーダー」の目安（環境依存の注記付き）と再現手順を記載。

### 残（任意）

- インスタンスプール枯渇時の挙動、WASM 燃料（fuel）消費量、RSS の自動計測は criterion の
  レイテンシ計測の範囲外。RSS は `/usr/bin/time -v`、fuel は Prometheus の
  `veil_wasm_filter_duration_*` 等で別途観測する（README に記載）。
- 「HTTP コールあり」フィルタのベンチは、外部 HTTP バックエンドのスタブが必要なため任意の
  継続課題（現状は header_filter による純フィルタ往復を計測）。
