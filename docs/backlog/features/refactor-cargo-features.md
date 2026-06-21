# F-23: Cargo.toml の features フラグ整理

## 概要
セキュリティサーフェスの削減、バイナリサイズ・コンパイル時間・ランタイムパフォーマンスの向上のために、`Cargo.toml` の `features` フラグを整理する。

## 要件
1. **デフォルト features の構成**
   `default` feature には以下の3つのみを指定する。
   - `ktls`
   - `http2`
   - `mimalloc`

2. **その他のオプショナル features の整理**
   - 圧縮関連 (`compression`: Gzip/Brotli/Zstd) や キャッシュ (`cache`: dashmap/lruなど)、メトリクス (`metrics`: prometheus)、WebSocket (`websocket`) などを個別の feature として切り出し、不要な場合はオフにできるようにする。
   - `full` などの便利な複合フィーチャも必要に応じて用意する。
   - all-protocolsは不要なので削除する
   - mimalloc・jemalloc・システムアロケータをfeaturesで切り替えられるようにする。デフォルトはmimallocを使用するようにする

## タスク
- [ ] `Cargo.toml` の `[features]` および `[dependencies]` セクションの修正
- [ ] `src/main.rs` (または `lib.rs`) での `cfg` 属性を使った機能の切り分け (必須機能は `cfg` なしで組み込む)
- [ ] `README.md` のビルドコマンド例の更新

## 備考
この整理により、`--no-default-features` を指定した場合でも必須のセキュリティ機能は維持しつつ、不要なモジュールを削ぎ落とした軽量ビルドが可能になる。


現在かなり高機能になっています。featuresのオプションになっていない機能について、オフにすることでセキュリティサーフェスの削減とパフォーマンス向上を検討しています。どのような機能をどんなfeaturesに分けるとよいですか

**はい、かなり高機能になっているので、features でオフにできるように整理するのは非常に良いアイデアです。**

セキュリティサーフェスの削減（特に複雑なライブラリの攻撃対象面積を減らす）と、バイナリサイズ・コンパイル時間・ランタイムパフォーマンスの向上の両面で効果が期待できます。

### 現在の状況（推測含む）
現在 `Cargo.toml` でオプショナルになっている主なもの：
- `ktls`, `http2`, `http3`, `wasm`, `grpc*` 系

**常に有効**になっている重めの機能（オフにしたい候補）：
- **Response Compression**（Gzip + Brotli + Zstd）
- **Proxy Cache**（dashmap + lru + xxhash-rust + glob）
- **Prometheus Metrics**
- **mimalloc + Huge Pages**
- **CPU Affinity**（core_affinity）
- **WebSocket** サポート
- **高度なセキュリティサンドボックス**（seccomp, Landlock, systemd）
- **高度な Buffering / Rate Limit / Header Manipulation** など

これらを features に切り出すと効果が大きいです。

### おすすめの features 整理案

```toml
[features]
default = ["ktls", "http2", "mimalloc"]   # 推奨デフォルト

# ============================================
# 既存のプロトコル・拡張系（そのまま or 微調整）
# ============================================
ktls = []
http2 = []
http3 = ["quiche"]
wasm = ["wasmtime"]
grpc = ["prost"]
grpc-web = ["grpc"]
grpc-full = ["grpc", "grpc-web", "http3"]

# ============================================
# 新規追加：セキュリティ・パフォーマンス向上向け
# ============================================

# 高速メモリアロケータ（mimalloc + Huge Pages）
mimalloc = ["dep:mimalloc", "dep:libmimalloc-sys"]

# レスポンス圧縮（Gzip/Brotli/Zstd 全部入り）
compression = ["flate2", "brotli", "zstd"]

# プロキシキャッシュ（メモリ + ディスク）
cache = ["dashmap", "lru", "xxhash-rust", "glob"]

# Prometheus メトリクス出力
metrics = ["prometheus"]

# WebSocket プロキシサポート
websocket = []

# レートリミット・接続制限
rate-limit = []

# 高度なレスポンスバッファリング制御
buffering = []

# ============================================
# 複合フィーチャ（便利用）
# ============================================

full = [
    "ktls", "http2", "http3",
    "grpc-full", "wasm",
    "compression", "cache", "metrics",
    "websocket", "rate-limit", "buffering",
    "mimalloc"
]
```

### 各 features の優先度と効果

| 優先度 | Feature            | 効果（セキュリティ/サイズ/パフォーマンス） | おすすめ度 | 備考 |
|--------|--------------------|-------------------------------------------|------------|------|
| ★★★★★ | `compression`     | 非常に高い（複雑な圧縮ライブラリを除外）   | 最高      | 最初に切り出すべき |
| ★★★★★ | `cache`           | 非常に高い（dashmap/lru が大きい）         | 最高      | キャッシュ不要なら大幅削減 |
| ★★★★☆ | `mimalloc`        | 良い（アロケータをシステムに戻せる）       | 非常に高い | default に含めるのがおすすめ |
| ★★★★  | `metrics`         | 良い（prometheus クレートを除外）          | 高い      | 多くの本番環境で不要 |
| ★★★☆  | `websocket`       | 中程度                                     | 中        | 必要なければオフ |
| ★★☆   | `rate-limit` / `buffering` | 中程度                                | 任意      | 必要に応じて |

### 実装時のポイント

1. **`mimalloc` の扱い方（重要）**
   ```toml
   [dependencies]
   mimalloc = { version = "...", optional = true, default-features = false }
   libmimalloc-sys = { version = "...", optional = true, features = ["extended"] }
   ```
   コード側で：
   ```rust
   #[cfg(feature = "mimalloc")]
   #[global_allocator]
   static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
   ```

2. **圧縮・キャッシュは `cfg(feature = "...")` でモジュールごと切り分けやすい**ので、比較的実装しやすいはずです。

3. **デフォルトの考え方**
   - `default = ["ktls", "http2", "mimalloc"]` がバランス良い。
   - 完全に最小構成にしたい人は `--no-default-features` でビルド可能にしておく。

4. **README の更新**
   - 「推奨ビルド」「最小構成（セキュリティ重視）」「フル機能ビルド」の3パターンを明記すると親切。

### 提案の優先順位（おすすめの進め方）

1. まず **`compression`** と **`cache`** を features 化（インパクト最大）
2. 次に **`mimalloc`** を optional + default に追加
3. その後 **`metrics`** と **`websocket`**
4. 最後に細かいセキュリティ系（`landlock` / `seccomp`） ※ セキュリティ系は必須なのでfeaturesでは無効にできないようにする

featuresフラグでビルド時にアロケータを変えることはできますか?mimalloc・jemalloc・システムアロケータを考えています、デフォルトではmimallocにしたいです

**はい、Cargoのfeaturesフラグで簡単に切り替え可能です。**

`veil-proxy` に以下の構成を追加・修正することをおすすめします（デフォルトは `mimalloc`）。

### 1. `Cargo.toml` の修正例

```toml
[features]
default = ["ktls", "http2", "mimalloc"]   # ← デフォルトで mimalloc

# アロケータ選択（排他的に1つだけ有効化）
mimalloc = ["dep:mimalloc"]
jemalloc = ["dep:tikv-jemallocator"]
system-allocator = []   # 何もしない（デフォルトSystemを使う）

# 他の既存features（ktls, http2 など）と併用可能
```

```toml
[dependencies]
# mimalloc（デフォルト）
mimalloc = { version = "0.1", optional = true, default-features = false }

# jemalloc
tikv-jemallocator = { version = "0.7", optional = true }

# 他の依存...
```

### 2. `src/main.rs`（または `lib.rs`）での切り替え

```rust
use cfg_if::cfg_if;

cfg_if! {
    if #[cfg(feature = "mimalloc")] {
        use mimalloc::MiMalloc;
        #[global_allocator]
        static GLOBAL: MiMalloc = MiMalloc;
        println!("Using mimalloc allocator"); // 任意：確認用

    } else if #[cfg(feature = "jemalloc")] {
        use tikv_jemallocator::Jemalloc;
        #[global_allocator]
        static GLOBAL: Jemalloc = Jemalloc;
        println!("Using jemalloc allocator");

    } else if #[cfg(feature = "system-allocator")] {
        // 明示的にSystem（何もしない）
        println!("Using system allocator");
    } else {
        // フォールバック（通常ここには来ない）
        compile_error!("Please enable one allocator feature: mimalloc, jemalloc, or system-allocator");
    }
}
```

**`cfg_if` クレート**を追加（すでにあれば不要）:
```toml
[dependencies]
cfg-if = "1"
```

### 3. ビルドコマンド例

```bash
# デフォルト（mimalloc）
cargo build --release

# jemalloc
cargo build --release --features jemalloc --no-default-features

# System allocator（明示的）
cargo build --release --features system-allocator --no-default-features

# 既存featuresと組み合わせ（例: kTLS + HTTP/2 + mimalloc）
cargo build --release --features "ktls,http2,mimalloc"
```

`--no-default-features` を付けることでデフォルトのmimallocを無効化できます。

### 4. 追加のTips（veil-proxy向け）

- **Huge Pages**：mimallocを使うときは環境変数 `MIMALLOC_ENABLE_HUGE_PAGES=1` を推奨（configやsystemdで設定）。
- **jemallocの高度な設定**：`tikv-jemalloc-ctl` もfeaturesで追加可能。
- **コンパイル時チェック**：上記の `compile_error!` で、必ずどれか1つを選ぶように強制できます。
- **CI / リリース**：GitHub Actionsなどで複数のアロケータ版バイナリをビルド・配布するのも簡単。

この構成にすれば、**デフォルトmimalloc**を保ちつつ、柔軟に切り替えられます。

実際に適用したい場合、具体的なCargo.tomlやmain.rsの該当部分を共有してもらえれば、**差分形式で正確な修正コード**をお渡しします！
