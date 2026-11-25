# F-320: HttpArena の veil エントリの設定を見直す（veil 本体を変えずに点を上げる）

**状態: 未着手（2026-11-22 起票。2026-11-25 に今の機能で試す調整〈7〜13〉を追加。F-317 の子。HttpArena への fork・push・PR・コメントは毎回、人の確認の後に行う〈F-269〉）**

## 今の状態

- エントリ（HttpArena の `frameworks/veil/`、手元の写しは `docs/artifacts/benchmark/entries/httparena/veil/`）は
  `ghcr.io/aofusa/veil:musl-v0.10.1`（epoll の `full-container`）の上に Proxy-Wasm のフィルタ（`httparena_filter.wasm`）を載せる。
  2026-11-24 の結果で 675 点（9 エントリ中 7 位。caddy 675 とほぼ同点）。
  - `[performance]`・全体の `[static_file_cache]` が無い → B-495 で open_file_cache が当たらず、B-496 で HTTP/2・HTTP/3 の静的配信が
    要求ごとに brotli で圧縮し直す。static-tls・static-h2・static-h3 が 97k・14k・14k rps（CPU は 11〜19 コアしか使っていない）。
  - `/pipeline`（本文 `ok`）もフィルタで答えている（pipelined 597k rps。veil-api は 3.43M）。okena も `/pipeline` は設定の
    `respond 200 "ok"` で返している。
  - HttpArena の `data/static/` には `.br`・`.gz` の兄弟が同梱されていて、nginx は `brotli_static on`・`gzip_static on`、okena は
    `precompressed=br,gz`、agensio も兄弟をキャッシュして配る。veil は事前圧縮を使っていない（F-294 は v0.11.0 から）。

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
4. **新しい版のイメージにする**: v0.11.0（事前圧縮）か、B-495・B-496・F-318 等を含む次の版。ノート PC で h1 の Wasm 無しの経路は
   v0.10.1 19.3 µs → main 12.3 µs。
5. **io_uring を測る**（採るかは計測で決める）: HttpArena はサーバのコンテナを `--security-opt seccomp=unconfined` で動かすので
   io_uring が使える。公開イメージは epoll だけなので、エントリの Dockerfile で既定の feature（io_uring）でビルドする版と比べる
   （crates.io の `veil-proxy` かタグのソース。ビルドの時間も見る）。okena は io_uring を持ちながら epoll を選んでいる
   （「送信を即時に書く。io_uring で全接続の暗号文が 1 回の提出を待つと static-h2 が 1.7 倍遅い」と設定に注記）。
6. 検証: HttpArena の `scripts/validate.sh`（veil の 11 テスト）を全部通す。

## 今の機能で試す調整（2026-11-25 追加。手元で測って良いものだけ採る）

7. **kTLS**（`[tls] ktls_enabled = true`、`ktls_fallback_enabled = true`）: static-tls・json-tls（HTTP/1.1 over TLS）で、静的配信は
   kTLS の上の `sendfile`、応答の暗号化はカーネルで行う。HttpArena の機械はカーネルの TLS が使える（connections-1m-tls で gcannon が
   kTLS を使っている）。HTTP/2 over TLS で使えるか・速いかも測る。
8. **暗号スイート**（`[tls] cipher_suites`）: rustls は既定でクライアントの順で選ぶ（veil は `ignore_client_order` を立てていない。
   文書の「記載順 = サーバ優先度順」は誤りで [B-498](../bugs/B-498-tls-cipher-suites-order-not-server-preference.md)）。h2load
   （HTTP/2・HTTP/3）は nghttp2 の既定の一覧（v1.69.0 の `DEFAULT_TLS13_CIPHER_LIST`）で AES-128-GCM を先頭に送るので、今も
   AES-128-GCM になっている。wrk（json-tls・static-tls）は OpenSSL 3.0 の既定（AES-256-GCM → ChaCha20 → AES-128-GCM）なので
   AES-256-GCM。今の機能で AES-128-GCM にするには TLS 1.3 の名前を `TLS13_AES_128_GCM_SHA256` だけにする（AES-256-GCM だけを外すと
   ChaCha20 が選ばれる）。B-498 を直せば順を並べるだけでよい。AES-128 は AES の段数が 10（AES-256 は 14）で、本文の大きい
   static-tls で効く見込み（測って決める）。
9. **接続の集約を切る**（`[performance] concentrate_connections = false`）: HTTP/2・HTTP/3 の接続は計測の最初に全部張られて動かない。
   B-497（一部の接続が止まる）との関係も見る。
10. **CPU の固定**（`[performance] cpu_affinity = "cores"`）: HttpArena のコンテナは専有の cpuset（0-31,64-95）。F-310 では専有の配置で
    差は 6% 以内だったので、測ってから決める。9 で集約を切ったときは `reuseport_balancing`（`"cbpf"`・`"kernel"`）も比べる。
11. **HTTP/3 の設定**: `[http3] initial_max_data`・`initial_max_stream_data_*`（static-h3 は 1 接続 64 ストリーム × 約 20 KB）、
    `mmsg_batch_size`、GSO（既定で有効）。`initial_max_streams_bidi`（既定 100）は h2load の `-m 64` に足りている。
12. **サンドボックスの費用**: seccomp のフィルタ（全 syscall で走る）・Landlock・特権の降格のある・なしで測る。差がはっきりしない
    （3% 未満）なら今のまま残す（nginx・okena 等はサンドボックスを使っていないが、veil の売りの 1 つなので残す方を優先する）。
13. 7〜12 はどれも「速くなったら採る」。計測は aofish で HttpArena の負荷ツール（wrk・h2load・h2load-h3・gcannon の Docker
    イメージ）を使い、HttpArena と同じ引数で行う（ノート PC は 4 CPU で、64 コアの傾向は出ない）。

## 手元の計測（ノート PC、4 CPU、veil-load。`docs/artifacts/benchmark/httparena-perf/results/m2.tsv`）

| 構成 | h1 静的 rps | h1 µs/要求 | h2c 静的 rps | h2c µs/要求 | 応答の平均 |
|---|---|---|---|---|---|
| v0.10.1・今の設定 | 18,514 | 60.7 | 770 | 1,463.7 | 22.7 KB |
| v0.10.1・1 の設定 | 30,158 | 37.2 | 38,835 | 29.0 | 22.7 KB |
| main・今の設定 | 23,240 | 48.3 | 710 | 1,587.5 | 19.0 KB |
| main・1 の設定 | 42,387 | 26.5 | 43,216 | 26.0 | 19.1 KB |
| main・1 + 2（事前圧縮） | 43,300 | 25.9 | 46,376 | 24.3 | 16.3 KB |

## 期待する効果（F-317 の試算。2026-11-24 の結果〈okena を含む〉で計算し直した）

- 1〜3: static の 3 プロファイルが veil-api 並み（937k・1.0M・289k）、pipelined が veil-api 並み（3.43M）なら、総合点 675 → 約 2,310
  （7 位 → 6 位）。veil-api の静的配信は File バックエンドではなくライブラリの `serve_dir` なので、この値は目安。64 コアでの本当の値は
  提出の前に aofish 等で確かめる。
- 7〜12 の効果は測ってから書く。

## 手順

1. `docs/artifacts/benchmark/entries/httparena/veil/` の設定を直し、手元で `validate.sh` と計測（aofish でも）。
2. 結果と差分を人に見せ、提出の可否を確かめる。
3. 可なら HttpArena の fork へ push・PR（毎回確認）。

## 関連

- [F-317](F-317-httparena-infra-score-plan.md)（親）・[B-495](../bugs/B-495-performance-section-default-zero-ofc.md)・
  [B-496](../bugs/B-496-content-cache-reinsert-drops-compressed.md)・[B-498](../bugs/B-498-tls-cipher-suites-order-not-server-preference.md)・
  [F-325](F-325-httparena-veil-compiled-handler-module.md)（ハンドラを組み込む版）・F-269・F-294・F-303・F-310・F-314
