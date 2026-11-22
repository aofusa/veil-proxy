# F-320: HttpArena の veil エントリの設定を見直す（veil 本体を変えずに点を上げる）

**状態: 未着手（2026-11-22 起票。F-317 の子。HttpArena への fork・push・PR・コメントは毎回、人の確認の後に行う〈F-269〉）**

## 今の状態

- エントリ（HttpArena の `frameworks/veil/`、手元の写しは `docs/artifacts/benchmark/entries/httparena/veil/`）は
  `ghcr.io/aofusa/veil:musl-v0.10.1`（epoll の `full-container`）の上に Proxy-Wasm のフィルタ（`httparena_filter.wasm`）を載せる。
  - `[performance]`・全体の `[static_file_cache]` が無い → B-495 で open_file_cache が当たらず、B-496 で HTTP/2・HTTP/3 の静的配信が
    要求ごとに brotli で圧縮し直す。static-tls・static-h2・static-h3 が 97k・14k・14k rps（CPU は 11〜19 コアしか使っていない）。
  - `/pipeline`（本文 `ok`）もフィルタで答えている（pipelined 597k rps。veil-api は 3.43M）。
  - HttpArena の `data/static/` には `.br`・`.gz` の兄弟が同梱されていて、nginx は `brotli_static on`・`gzip_static on` で配る
    （その場の圧縮は品質 1）。veil は事前圧縮を使っていない（F-294 は v0.11.0 から）。

## 改修内容（設定とイメージの版だけ）

1. **キャッシュの全体の設定を書く**（B-495・B-496 の回避。v0.10.1 でも効く）:
   ```toml
   [performance]
   open_file_cache_enabled = true
   open_file_cache_watch = true

   [static_file_cache]
   enabled = true
   watch = true
   ```
2. **`/static/*` に事前圧縮を使う**（v0.11.0 以降のイメージが要る。nginx の `brotli_static`・`gzip_static` と同じ）:
   `[route.action] precompressed = ["br", "gzip"]`。
3. **`/pipeline` を設定の固定応答で返す**（Wasm を通さない。検証は本文 `ok` と `Content-Type: text/plain`。infrastructure の規則は
   「設定の制限なし」）:
   ```toml
   [[route]]
   [route.conditions]
   path = "/pipeline"
   [route.action]
   type = "Respond"
   status = 200
   body = "ok"
   content_type = "text/plain"
   ```
4. **新しい版のイメージにする**: v0.11.0（事前圧縮）か、B-495・B-496・F-318 を含む次の版。ノート PC で h1 の Wasm 無しの経路は
   v0.10.1 19.3 µs → main 12.3 µs。
5. **io_uring を測る**（採るかは計測で決める）: HttpArena はサーバのコンテナを `--security-opt seccomp=unconfined` で動かすので
   io_uring が使える。公開イメージは epoll だけなので、エントリの Dockerfile で既定の feature（io_uring）でビルドする版と比べる
   （crates.io の `veil-proxy` かタグのソース。ビルドの時間も見る）。
6. 検証: HttpArena の `scripts/validate.sh`（veil の 11 テスト）を全部通す。

## 手元の計測（ノート PC、4 CPU、veil-load。`docs/artifacts/benchmark/httparena-perf/results/m2.tsv`）

| 構成 | h1 静的 rps | h1 µs/要求 | h2c 静的 rps | h2c µs/要求 | 応答の平均 |
|---|---|---|---|---|---|
| v0.10.1・今の設定 | 18,514 | 60.7 | 770 | 1,463.7 | 22.7 KB |
| v0.10.1・1 の設定 | 30,158 | 37.2 | 38,835 | 29.0 | 22.7 KB |
| main・今の設定 | 23,240 | 48.3 | 710 | 1,587.5 | 19.0 KB |
| main・1 の設定 | 42,387 | 26.5 | 43,216 | 26.0 | 19.1 KB |
| main・1 + 2（事前圧縮） | 43,300 | 25.9 | 46,376 | 24.3 | 16.3 KB |

## 期待する効果（F-317 の試算）

- 1〜2: static の 3 プロファイルが veil-api 並み（937k・1.0M・289k）になれば、総合点 771 → 約 2,480（8 エントリ中 7 位 → 5 位）。
  veil-api の静的配信は File バックエンドではなくライブラリの `serve_dir` なので、この値は目安。64 コアでの本当の値は提出の前に
  aofish 等で確かめる。
- 3: pipelined が veil-api 並み（3.4M）なら +約 320（約 2,800）。

## 手順

1. `docs/artifacts/benchmark/entries/httparena/veil/` の設定を直し、手元で `validate.sh` と計測（aofish でも）。
2. 結果と差分を人に見せ、提出の可否を確かめる。
3. 可なら HttpArena の fork へ push・PR（毎回確認）。

## 関連

- [F-317](F-317-httparena-infra-score-plan.md)（親）・[B-495](../bugs/B-495-performance-section-default-zero-ofc.md)・
  [B-496](../bugs/B-496-content-cache-reinsert-drops-compressed.md)・F-269・F-294
