# F-320: HttpArena の veil エントリの設定を見直す（veil 本体を変えずに点を上げる）

**状態: 完了（2026-11-26、手元。1〜5・8 を入れ、7・9〜12 は測って採らなかった。HttpArena の validate.sh は 72 件すべて通過。提出〈手順の 2・3〉は人の確認の後〈F-269〉。F-317 の子）**

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

## 進捗（2026-11-26。手元の写し `docs/artifacts/benchmark/entries/httparena/veil/`）

### 入れたもの

- 1〜3: `[performance]`・全体の `[static_file_cache]`、`/static/*` の `precompressed = ["br", "gzip"]`、`/pipeline` の `Respond`。
- 4・5: Dockerfile は crates.io の次の版（F-318〜F-325・B-495〜B-501 を含む版）を `--features full`（io_uring）・`--profile dist`
  でビルドし、debian:bookworm-slim に nonroot（65532）と一緒に置く形にした（veil の公開イメージは epoll）。io_uring にしたのは
  F-325 の計測（h1 +54%、試算の点で +320）。手元の検証用に、作業ツリーでビルドしたバイナリを入れる `Dockerfile.local` も置いた。
- 8: `[tls] cipher_suites` を AES-128-GCM を先頭に並べた（B-498 で記載順がサーバの優先になった）。
- 7・9〜12 は採らない（下の計測）。

### aofish の計測（86297922、交互 3 巡の中央値。rps。全部の値は F-325 の表）

| 負荷 | v0.10.1 の今のエントリ（epoll） | この設定（epoll） | この設定（io_uring） |
|---|---|---|---|
| h1（`/baseline11`） | 129k | 283k | 436k |
| パイプライン（`/pipeline`） | 202k | 1.49M | 1.56M |
| h2c・h2（TLS） | 359k・352k | 1.23M・1.03M | 1.27M・1.05M |
| h3 | 95k | 130k | 116k |
| json-tls・static-tls | 110k・87k | 198k・213k | 193k・222k |
| static-h2・static-h3 | 5k・1.3k | 392k・35k | 376k・32k |

- 試算（F-325 の 3 と同じ方法）: 675 → 約 3,580（io_uring）・約 3,260（epoll）。6 位で、h2o（3,874）には届かない。

### 調整（7〜12）の計測（組み込みの版〈F-325〉の io_uring で、交互 2 巡。1 要求の CPU の変化、負が良い）

| 調整 | h1 | パイプライン | h2c | h2 | h3 | json-tls | static-tls | static-h2 | static-h3 |
|---|---|---|---|---|---|---|---|---|---|
| 7 kTLS | +2.7% | +5.0% | +4.8% | +0.6% | +12.9% | **+80.1%** | **+42.5%** | **+68.3%** | +5.3% |
| 8 AES-128-GCM を先頭 | +2.1% | +2.6% | +1.0% | −0.7% | +0.3% | −3.2% | −5.2% | −0.6% | −5.6% |
| 9 接続の集約を切る | +0.0% | −1.7% | −1.0% | +0.1% | −2.7% | +1.0% | −2.8% | +0.7% | −1.8% |
| 10 CPU の固定 | −0.9% | +3.8% | +3.6% | +2.4% | −0.1% | −2.7% | +2.8% | +3.3% | −3.9% |
| 12 サンドボックス無し | +1.1% | +0.3% | −0.8% | +0.6% | −0.3% | −2.3% | +1.1% | +1.7% | −4.2% |
| 11 `mmsg_batch_size = 256` | −2.2% | +0.8% | −0.8% | −0.4% | **+12.1%** | +4.7% | −1.5% | +0.7% | −3.3% |
| （基準の µs） | 10.88 | 3.28 | 2.62 | 3.51 | 5.14 | 18.38 | 31.38 | 18.51 | 26.83 |

- 揺れの大きさ: 7・8 の平文の負荷（h1・パイプライン・h2c。TLS の設定は効かない）は ±5% 動いている。この幅に収まるものは差が無いと見る。
- 7 kTLS: TLS の 3 つで 1 要求の CPU が 1.4〜1.8 倍。aofish の WSL2 のカーネル（6.6、`CONFIG_TLS=m`、ソフトウェアの kTLS）の
  結果で、HttpArena の機械で同じとは限らないが、測って速くならなかったので採らない。
- 8 AES-128-GCM を先頭: wrk の 2 つ（OpenSSL の既定は AES-256-GCM が先頭）で −3.2%・−5.2%（ent2 の同じ版と比べると −1%・−2%）。
  揺れと同じ程度だが、向きは理屈（AES の段数 10 と 14）と合い、副作用も無いので採る。h2load は元から AES-128-GCM を先頭に出すので
  h2 は変わらない。
- 9・10・12: 全部が揺れの中。9・10 は既定のまま（F-310 と同じ結果）、12 は veil の売りの 1 つなので残す。
- 11: `mmsg_batch_size = 256` は h3 で +12% と悪い。流量の窓（`initial_max_data` 10 MB・ストリームごと 1 MB）は static-h3 の
  1 接続 64 ストリーム × 約 20 KB に足りているので変えない。

### HttpArena の検証（validate.sh）

- aofish の Docker Desktop で HttpArena（3388e45）の `scripts/validate.sh` を `VALIDATE_SKIP_BUILD=1` で回した。イメージは
  `Dockerfile.local`（ubuntu:24.04 に、86297922 でビルドした io_uring のバイナリと Proxy-Wasm のフィルタ・`veil.toml` を入れたもの）。
  **72 件すべて通過（失敗 0・スキップ 0）**: HTTP/1.1・パイプライン・TLS・h2・h2c・HTTP/3（h2load-h3 で QUIC）・静的ファイルの
  差し替えへの追従・127.0.0.160 からの接続・TLS の判定。
- 手元の写しの validate.sh は HTTP/3 の検査 2 つの URL を `localhost` から `127.0.0.1` に替えて回した（Docker Desktop の VM では
  `localhost` が先に `::1` になり、公開した UDP のポートは IPv4 だけのため。TCP の検査は `localhost` のまま）。

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
- 7〜12 の効果は「進捗」の表（採ったのは 8 だけ）。測った値での試算は 675 → 約 3,580（io_uring。「進捗」の aofish の計測）。

## 手順

1. `docs/artifacts/benchmark/entries/httparena/veil/` の設定を直し、手元で `validate.sh` と計測（aofish でも）。（済み。2026-11-26）
2. 結果と差分を人に見せ、提出の可否を確かめる。
3. 可なら HttpArena の fork へ push・PR（毎回確認）。

## 関連

- [F-317](F-317-httparena-infra-score-plan.md)（親）・[B-495](../bugs/B-495-performance-section-default-zero-ofc.md)・
  [B-496](../bugs/B-496-content-cache-reinsert-drops-compressed.md)・[B-498](../bugs/B-498-tls-cipher-suites-order-not-server-preference.md)・
  [F-325](F-325-httparena-veil-compiled-handler-module.md)（ハンドラを組み込む版）・F-269・F-294・F-303・F-310・F-314
