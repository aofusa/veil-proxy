# F-159: ルート解決（`load_backend` / `find_backend_unified`）のリクエスト単価ゼロアロケーション化

**優先度**: P1
**ステータス**: 完了（実測 h2c_file 3B **+7.41%** / h2c_proxy 54KB **+2.65%**、いずれも 6/6 ラウンドで勝ち）
**関連**: F-148（`Route::resolved_modules` の事前解決）、B-64（`load_backend` のホットパス性）

---

## 背景

`upstream::find_backend_unified` は **1 リクエストにつき 1 回以上**呼ばれ、その中で
`config::load_backend` を呼ぶ。F-148 で WASM モジュールリストだけは設定ロード時の
事前解決（`Arc` 共有）にしたが、**残り全部はリクエストごとに作り直していた**。

| 箇所 | リクエストごとのコスト |
|---|---|
| `load_backend` 冒頭 | `security` / `compression` / `buffering` / `cache` の `Option<T>` ディープコピー 4 個 |
| Proxy 分岐 | `ProxyTarget::parse(url)`（String 確保）、`UpstreamGroup::single(target)` + `Arc::new` |
| 各分岐 | `Arc::new(security.clone())` 等で **もう一度**ディープコピー + Arc 確保（最大 5 個） |
| File 分岐 | `Arc::new(PathBuf::from(path))` / `Arc::from(index)` / `open_file_cache`・`static_file_cache` の `Arc::new(clone)` |
| File(memory) 分岐 | **`fs::read(path)` をリクエストごとに実行**（ファイル全体の読み込み） |
| 各分岐 | 圧縮・キャッシュ有効時の `info!` ログ（リクエストごとに出力） |
| `find_backend_unified` | `extract_path_prefix`（`Box<[u8]>` 確保）、`Arc::new(compression.clone())` |

合計で **10 個以上のアロケーション + 複数のディープコピー / リクエスト**。ホットパス
絶対規則（アロケーション禁止）に真っ向から反していた。

## 改修内容

F-148 と同じ形（`#[serde(skip)]` の解決済みフィールドを設定ロード時に埋める）を
**`Backend` 全体へ拡張**した。

- `Route` に `resolved_backend: Option<Backend>` / `resolved_compression: Option<Arc<CompressionConfig>>` /
  `resolved_path_prefix: Option<Arc<[u8]>>` を追加（すべて `#[serde(skip)]`）。
- 設定ロードの 2 経路（起動時 `load_config` / SIGHUP リロード `load_config_without_tls`）で、
  `resolve_route_modules` の直後に上記 3 つを構築する。
- `load_backend` は「解決済みなら `clone()` して返すだけ」の薄いラッパーになり、実体は
  `build_backend`（非公開）へ改名（**中身は 1 行も変えていない**）。`Backend` の全バリアントは
  `Arc` / `Option<Arc>` / `Copy` のみを持つため、`clone()` は参照カウント増分だけで malloc しない。
- `find_backend_unified` / `find_backend_linear` は解決済みの prefix / compression を `Arc` clone
  するだけになった（戻り値の prefix は `Box<[u8]>` → `Arc<[u8]>`）。

**解決に失敗したルート**（存在しないファイルパス等）は `resolved_backend` を `None` のままにし、
ホットパスは従来どおり毎回 `build_backend` を呼んで**同じエラーを返す**（後方互換の安全網）。

## 挙動変更

- `mode = "memory"` の File バックエンドは**設定ロード時にファイル内容を読み込む**ようになった
  （従来はリクエストごとに `fs::read`）。ファイルを差し替えたら SIGHUP が必要。
  これは memory モード本来の意図（メモリから返す）に沿った修正である。
- 圧縮・バッファリング・キャッシュ有効時の `info!` ログが設定ロード時 1 回になった。

## 計測（交互 A/B、Linux x86_64 4 コア quiet host、io_uring 既定ビルド）

| 構成 | base 中央値 | new 中央値 | 差 | new 勝ち |
|---|---|---|---|---|
| h2c_proxy（54,576B） | 10,321.4 rps | 10,594.5 rps | **+2.65%** | 6/6 |
| h2c_file（3B） | 95,706.8 rps | 102,796.4 rps | **+7.41%** | 6/6 |

固定費が支配する 3B のほうが改善幅が大きく、削ったものがリクエスト単価のアロケーション
であることと整合する。生データは `docs/perf/results_raw.tsv` の該当節。
