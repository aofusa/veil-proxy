# F-169: 静的配信の圧縮結果キャッシュと圧縮コンテキストの再利用

## 背景（実測）

B-76 を修正したあと、`tools/perf` の Linux フルスイートで **nginx ベースライン
（TLS 静的、HTTP/1.1 6,714 rps / HTTP/2 6,204 rps）に対して明確に劣後する構成は
圧縮系だけ**になった（他はすべて 0.93〜1.00× で、しかもそれらは逆プロキシ構成 ⇔
nginx 静的配信という非同条件比較）。

| 構成 | ルート種別 | プロトコル | veil | 対 nginx |
|---|---|---|---|---|
| `h3_proxy_compression` | Proxy | HTTP/1.1 | 1,789 | **0.27×** |
| `h2_1_proxy_compression` | Proxy | HTTP/1.1 | 2,063 | **0.31×** |
| `h3_proxy_compression` | Proxy | HTTP/2 | 2,064 | **0.33×** |
| `h2_1_proxy_compression` | Proxy | HTTP/2 | 2,181 | **0.35×** |
| **`h2_1_feat_compression`** | **File（静的）** | HTTP/2 | **2,324** | **0.37×** |
| **`h3_file_compression`** | **File（静的）** | HTTP/2 | **2,554** | **0.41×** |

計測条件: 54,576B のレスポンス、クライアントは `accept-encoding: gzip, br, zstd`
（`run_perf.sh` が compression 構成にのみ付与）、`preferred_encodings = ["zstd","br","gzip"]`
なので **zstd（既定 `zstd_level = 3`）** が選択される。

## 原因

`src/proxy.rs` の `compress_body_h2`（および `http3_server.rs` の `compress_body_h3`）が

```rust
zstd::encode_all(std::io::Cursor::new(body), compression.zstd_level)
```

を **リクエストごとに** 呼んでいる。問題は 2 つある。

1. **静的ファイルは毎回まったく同じ内容を再圧縮している。** 非圧縮の本体は
   F-157 以降 `static_file_cache` から `Bytes` の参照カウントクローンでゼロコピーに
   取れるようになっているのに、その直後で 54KB を毎リクエスト zstd 圧縮している。
   nginx は `gzip_static`（事前圧縮ファイル）でこれを回避する。
2. **`zstd::encode_all` は 1 回ごとに圧縮コンテキストを作り捨てている。**
   zstd のコンテキスト確保はワークスペース確保を伴い高価で、`zstd::bulk::Compressor`
   を使い回せば消せる。プロキシ経路（応答が毎回異なるため 1 のキャッシュが使えない）
   でもこちらは効く。

## 改修

### A. 静的配信の圧縮結果キャッシュ（本命）

`(パス, エンコーディング, 圧縮レベル)` をキーに圧縮済み `Bytes` を保持するキャッシュを
追加し、静的配信の圧縮をキャッシュヒット時ゼロコストにする。

- **有効化条件は `static_file_cache`（本体キャッシュ）と同じ**とする。
  F-157 の教訓（「メタデータキャッシュと本体キャッシュはセットで有効にしないと
  素通りする」）を繰り返さないよう、**本体キャッシュが有効なときだけ**圧縮キャッシュも効く、
  という単純な依存関係にする。
- **無効化は本体キャッシュと連動させる。** `cache::invalidate_content_cache(path)` が
  呼ばれたら同じパスの圧縮結果も必ず捨てる（mtime 再検証で本体が入れ替わったのに
  古い圧縮結果を返す、という不整合を作らない）。**ここが本改修で最も壊しやすい点**。
- 上限は本体キャッシュと同じ設定値（`max_entries` / `max_total_bytes`）の考え方に揃え、
  圧縮結果ぶんのメモリが青天井にならないようにする。
- 対象は **File ルートの静的配信のみ**（HTTP/2 と HTTP/3）。プロキシ応答は対象外。

### B. 圧縮コンテキストの再利用（プロキシ経路にも効く）

`zstd::encode_all` をスレッドローカルに保持した `zstd::bulk::Compressor` の使い回しへ
置き換える。ホットパス絶対規則（リクエストごとの確保を増やさない）にも沿う。

**注意**: AGENTS.md の F-168 の教訓どおり、**「確保が減った」と「速くなった」は別の主張**である。
B は交互 A/B で µs/req の改善が計測ノイズを超えるかを確認し、超えないなら
「設計の一貫性」を根拠に採否を判断する（スループット改善を約束しない）。
**A は理論上 54KB の zstd 圧縮そのものが消えるので、桁で効くはず**である。

## 完了条件

- `h2_1_feat_compression` / `h3_file_compression`（静的 + 圧縮）が
  **対 nginx で 1.0× を超える**こと。
- 非圧縮経路（`h2c_file` / `h2c_proxy` / 直交表）に退行が無いこと。
- 単体 / 統合 / E2E がすべて pass すること。
- README（en/ja）・`examples/config.toml`・`contrib/config/config.toml` に
  圧縮キャッシュの挙動とメモリ上限を明記すること。
