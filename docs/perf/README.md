# docs/perf — パフォーマンス計測サマリ（v0.12.0）

v0.12.0 は、API ライブラリの経路の 4 領域（JSON + 圧縮・コア制限下の遅延・unary gRPC・Python のハンドラ）と、既定の Linux ビルド
（io_uring）の 1 要求あたりの費用を改善した（[F-295](../backlog/features/F-295-v0120-perf-harness.md) とその子チケット）。
ここには、前の版の v0.11.0 と交互に測った **最新の結果だけ** をまとめる（過去の計測は git 履歴で辿れる）。

| ファイル | 内容 |
|---|---|
| [`linux_api_ab_raw.tsv`](linux_api_ab_raw.tsv) | API の経路（`tools/perf/api_ab.py` の json・latency・grpc・python）の v0.11.0 との交互 A/B の生データ |
| [`linux_python_bench_raw.tsv`](linux_python_bench_raw.tsv) | Python のハンドラと FastAPI（uvicorn）の比較（`tools/api-bench/run_bench.py --group python`）の生データ |
| [`linux_python_i5_raw.tsv`](linux_python_i5_raw.tsv) | Python の目標値を測った機械（i5-3320M）での v0.11.0 との交互 A/B（`veil-load`）の生データ |
| [`linux_loadgen_calib_raw.tsv`](linux_loadgen_calib_raw.tsv) | 負荷生成の道具（`wrk` と `veil-load`）と置き場所の較正の生データ |
| [`linux_proxy_ab_raw.tsv`](linux_proxy_ab_raw.tsv) | プロキシ本体（`tools/perf/proxy_ab.py`。HTTP/1.1・TLS・h2c・HTTP/2・HTTP/3 の静的配信と中継）の交互 A/B の生データ |
| [`linux_suite_ab_raw.tsv`](linux_suite_ab_raw.tsv) | Docker のフルスイート（`suite_ab.sh`。v0.11.0 と交互、nginx の同じ仕事の相手を含む）の生データ |
| [`linux_remeasure_raw.tsv`](linux_remeasure_raw.tsv) | 上で v0.11.0 との比が 0.95 を下回った 9 構成を、順を入れ替えて 5 回測り直した生データ |
| [`linux_native_log_ktls_raw.tsv`](linux_native_log_ktls_raw.tsv) | Docker を使わないアクセスログの有無・kTLS・スイートと同じ形のワーカー数の交互 A/B（`proxy_ab.py`）の生データ（`wrk_timeouts` が 0 より大きい行は壁時計の戻りが入っているので集計から除く） |
| [`freebsd_v0110_ab_raw.tsv`](freebsd_v0110_ab_raw.tsv) | FreeBSD の v0.11.0 との交互 A/B（rps・CPU/要求）の生データ |
| [`freebsd_results_raw.tsv`](freebsd_results_raw.tsv) | FreeBSD 14.3 aarch64 の `run_perf_bsd.sh`（標準・サーバ律速 × 54KB・3B、nginx と）の生データ |
| [`platform_verification.md`](platform_verification.md) | 全プラットフォームの試験（v0.12.0） |
| [`linux_beat_suite_raw.tsv`](linux_beat_suite_raw.tsv) | feat/beat-nginx のフルスイート（lab51。v0.12.0 と交互、nginx の同じ仕事の相手を含む。下の「feat/beat-nginx」）の生データ |
| [`linux_beat_remeasure_raw.tsv`](linux_beat_remeasure_raw.tsv) | 上で v0.12.0 との比が 0.95 を下回った組の測り直し（lab58・lab61）の生データ |
| [`linux_beat_syscall_latency_raw.tsv`](linux_beat_syscall_latency_raw.tsv) | 1 要求あたりの syscall の数（lab56）と低い並列度（1・10 接続）の応答時間（lab59。nginx と）の生データ |
| [`linux_beat_h3_raw.tsv`](linux_beat_h3_raw.tsv) | HTTP/3 のサーバ律速・負荷側 8 スレッド・低い並列度の nginx との比較（lab56）の生データ |
| [`linux_beat_churn_raw.tsv`](linux_beat_churn_raw.tsv) | 接続を毎回張り直す負荷（lab60・lab62。OpenSSL 3 と LibreSSL 2.7 の wrk、nginx と）の生データ |

---

## feat/beat-nginx（v0.12.0 の後、2026-11-11〜14）

コンテナ間の HTTP/1.1 で nginx を下回っていた原因（負荷の少ない接続が全ワーカーへ散って、ワーカーごとの起床・
`io_uring_enter` が要求の数に近くなる）を、接続の集約（[F-303](../backlog/features/F-303-concentrate-connections.md)。
SO_REUSEPORT のグループの CBPF を制御スレッドが差し替えて、新しい接続を先頭の N 本のワーカーへ振り分ける）で直した。
あわせて静的配信のキャッシュに載ったファイルの I/O をワーカーの上で行い（[F-304](../backlog/features/F-304-inline-cached-file-io.md)）、
HTTP/3 の GSO を既定にし（[F-305](../backlog/features/F-305-h3-gso-default.md)・[B-361](../backlog/bugs/B-361-h3-gso-socket-default-splits-single-packets.md)）、
QUIC のパケット番号を飛ばす頻度を下げ（[F-306](../backlog/features/F-306-h3-pn-skip-policy.md)）、中継の応答の写しを減らし
（[F-307](../backlog/features/F-307-proxy-response-copy-removal.md)）、HTTP/3 の応答の最後の短いパケットを GSO の束に入れ
（[F-309](../backlog/features/F-309-h3-gso-short-tail.md)）、ワーカーを既定で CPU に固定しないようにし
（[F-310](../backlog/features/F-310-cpu-affinity-off-by-default.md)）、io_uring の HTTP/3 の受信で空のソケットに並べる待ちを 1 本にした
（[F-311](../backlog/features/F-311-h3-single-recv-waiter.md)）。途中で見つけた
[B-360](../backlog/bugs/B-360-h1-proxy-close-without-connection-close.md)・[B-362](../backlog/bugs/B-362-h3-first-batch-body-skips-size-limit.md)・
[B-366](../backlog/bugs/B-366-concentrate-h2-burst-after-pause.md)、セキュリティの検査で見つけた B-368〜B-399、TLS の接続を close_notify
無しで閉じていた [B-401](../backlog/bugs/B-401-tls-close-notify.md)、HTTP/3 の起動直後の競合の
[B-402](../backlog/bugs/B-402-http3-udp-bind-race-at-startup.md) も直した。

計測は aofish の Docker Desktop のコンテナ間（下の「計測条件」と同じ）。フルスイートは feat/beat-nginx の最終版（`veil:beatfinal`。
B-401・B-402 の前。どちらも接続を閉じるとき・起動のときだけの変更で、キープアライブのスイートには効かない）を v0.12.0
（`veil:v0120-uring`）と交互に、`nginx:alpine`（`quic_gso on`）と 69 構成 × 3 反復（lab51）。全 834 計測で Non-2xx = 0。

### nginx との比較（同じ仕事の相手、Req/s 中央値の比。同じ回の v0.12.0 と並べる）

| プロトコル | 構成数 | v0.12.0 | feat/beat-nginx | 最小 | nginx 未満 |
|---|---|---|---|---|---|
| HTTP/1.1 | 54 | 0.75（未満 31） | **1.43** | 0.99 | 1（0.99） |
| HTTP/2 | 45 | 1.43（未満 1） | **1.48** | 1.27 | 0 |
| HTTP/3 | 19 | 0.57（未満 19） | **1.25** | 1.10 | 0 |
| h2c | 2 | 1.83 | **1.83** | 1.74 | 0 |

代表的な構成（feat/beat-nginx。括弧は同じ回の v0.12.0 の対 nginx）:

| 構成 | プロトコル | nginx | veil | 対 nginx |
|---|---|---|---|---|
| `h2_1_ktls_0_lb_kernel_ofc_1`（静的） | HTTP/1.1 | 54,699 | 88,053 | **1.61×**（0.81×） |
| `h2_0_ktls_0_lb_cbpf_ofc_0`（静的、ファイルキャッシュ無し） | HTTP/1.1 | 54,699 | 80,155 | **1.47×**（0.48×） |
| `h2_1_feat_proxy`（中継） | HTTP/1.1 | 32,771 | 46,369 | **1.41×**（1.22×） |
| `h2_1_feat_proxy`（中継） | HTTP/2 | 21,649 | 39,392 | **1.82×**（1.80×） |
| `h2_1_ktls_0_lb_cbpf_ofc_1`（静的） | HTTP/2 | 30,984 | 43,875 | **1.42×**（1.43×） |
| `h2_1_proxy_compression`（中継 + 圧縮） | HTTP/2 | 6,640 | 23,315 | **3.51×**（3.48×） |
| `h2_1_feat_compression`（毎回圧縮） | HTTP/1.1 | 10,794 | 32,707 | **3.03×**（2.30×） |
| `h3_proxy_cache`（中継 + キャッシュ） | HTTP/1.1 | 32,771 | 83,035 | **2.53×**（1.44×） |
| `h2_0_feat_l4`（L4 平文） | HTTP/1.1 | 54,699 | 72,114 | **1.32×**（1.31×） |
| `h2c_file` / `h2c_proxy` | h2c | 65,661 / 36,032 | 126,621 / 62,647 | **1.93× / 1.74×** |
| `h3_file_metrics`（静的） | HTTP/3 | 34,940 | 45,029 | **1.29×**（0.54×） |
| `h3_proxy`（中継） | HTTP/3 | 30,578 | 35,913 | **1.17×**（0.55×） |

- HTTP/1.1 で nginx を下回った 1 構成は kTLS 有りでファイルキャッシュ無しの静的配信（`h2_0_ktls_1_lb_cbpf_ofc_0`、54,207 対
  54,699）で、nginx の `base` の反復ごとの揺れ（52,788〜61,703）の内側。kTLS 有りの静的配信の 8 構成は 0.99〜1.11。
- HTTP/3 は 19 構成のすべてで nginx を上回った（機能を足した中継の `h3_proxy_otel` 1.10・`_buffering` 1.13 も。nginx の相手
  `base_proxy` はこれらの機能を持たない）。

### v0.12.0 との退行確認（交互 A/B）

lab51 の 133 組の比の中央値は **1.324**（HTTP/1.1 1.901、HTTP/2 1.085、HTTP/3 2.104、gRPC 0.995、gRPC over HTTP/3 1.068、h2c 1.004、
WebSocket 1.001）。比が 0.95 を下回った 4 組は、ビルドを並べてラウンドごとに順を回して測り直した（lab58 は 4 ラウンド、lab61 は
k6 を 20 秒にして 8 ラウンド）。

| 構成 | プロトコル | lab51 | 測り直し |
|---|---|---|---|
| `h2_1_feat_grpc` | gRPC | 0.884 | lab61 で 0.975（lab51 と同じビルド）・1.003（B-401 の後）。ラウンドごとの比は 0.66〜1.16 と揺れる。サーバの CPU / 要求は v0.12.0 の 166 µs に対して 158〜164 µs |
| `grpc_h2_metrics` | gRPC | 0.885 | lab58 で 1.082、lab61 で 1.069・1.044。CPU / 要求は 173 → 159〜163 µs |
| `h3_file_compression_cached`・`h2_1_feat_compression_cached` | HTTP/2 | 0.948・0.950 | lab58 で 1.035。2 つは名前だけが違う同じ設定で、合わせた 8 回の中央値は 0.97 |

- gRPC の行は負荷（k6）と上流（grpcbin）が律速で、ビルドの差より回ごとの揺れが大きい。

### 1 要求あたりの syscall と応答時間（HTTP/1.1 + TLS）

syscall の数は perf のトレースポイント（`raw_syscalls:sys_enter`）をサーバのプロセスで数えた（`wrk -t4 -c100`、lab56、3 回の中央値）。
応答時間は `wrk --latency` の 1 接続・10 接続（`-t1 -c1`・`-t2 -c10`、lab59、5 回の中央値）。静的配信は 54KB
（`h2_1_ktls_0_lb_kernel_ofc_1`）、中継は `h2_1_feat_proxy`。

| 測るもの | 静的配信 nginx | 静的配信 veil | 中継 nginx | 中継 veil |
|---|---:|---:|---:|---:|
| syscall / 要求（100 接続） | 14.96 | **3.46** | 22.37 | **3.89** |
| Req/s（同じ回、100 接続） | 49,998 | **80,513** | 32,565 | **44,679** |
| 1 接続の p50 / p99（ms） | 0.152 / 0.587 | 0.160 / **0.382** | 0.317 / 0.842 | 0.323 / **0.722** |
| 1 接続の Req/s | 5,172 | **5,579** | 2,720 | 2,731 |
| 10 接続の p50 / p99（ms） | 0.365 / 0.741 | **0.264 / 0.622** | 0.627 / 1.120 | **0.545 / 0.990** |
| 10 接続の Req/s | 24,882 | **34,364** | 14,964 | **17,371** |

- 1 接続の中継は Req/s が同じ（1.00×）で、p50 は veil が 6 µs 遅く、p99 は 14% 低い。1 接続の静的配信も p50 は 8 µs 遅いが、
  p90・p99 は低く Req/s は 1.08×。
- veil は要求ごとの読み書き・上流への送受信を io_uring の SQE にまとめて、提出を実行器の park でまとめて行う（AGENTS.md の
  io_uring の規則）ので、syscall の数は nginx（epoll + `read`・`write`・`writev`）の 4 分の 1〜6 分の 1。

### HTTP/3（lab56、3 回の中央値）

| 配置 | nginx | veil | 対 nginx | サーバの CPU / 要求（nginx → veil） |
|---|---:|---:|---:|---|
| サーバ律速・静的配信（サーバ 4 CPU・4 ワーカー、h2load 12 CPU・8 スレッド） | 27,308 | 38,290 | **1.40×** | 140 → 100 µs |
| サーバ律速・中継 | 15,459 | 28,621 | **1.85×** | 277 → 133 µs |
| 標準の配置・h2load 8 スレッド・静的配信 | 47,168 | 54,286 | **1.15×** | 188 → 150 µs |
| 1 接続 × 1 ストリーム | 3,108 | 3,348 | **1.08×** | |
| 10 接続 × 1 ストリーム | 16,973 | 18,626 | **1.10×** | |

- スイートと同じ h2load の 4 スレッドでは負荷側が律速。パケット番号を飛ばす頻度（F-306）は負荷側（ngtcp2）の費用に効く。
  既定の `on_growth` で負荷側の 1 要求の CPU は nginx と同じになった（lab22・lab23 の結果は F-306 のチケット）。

### 接続を毎回張り直す負荷（lab60・lab62、HTTP/1.1 + TLS、54KB、100 接続、`Connection: close`、3 回の中央値）

| 負荷 | nginx | veil | 対 nginx | サーバの CPU / 要求（nginx → veil） |
|---|---:|---:|---:|---|
| OpenSSL 3 の wrk（`local/wrk-openssl`、TLS 1.3 のチケットで再開） | 5,542 | 6,360 | **1.15×** | 874 → 464 µs |
| 同、再開しない（nginx は `ssl_session_tickets off`、veil は `session_resumption = "off"`） | 1,991 | 2,306 | **1.16×** | 1,270 → 661 µs |
| LibreSSL 2.7 の wrk（`williamyeh/wrk`、TLS 1.2）、veil は既定の `"tickets"` | 9,126 | 3,934 | 0.43× | 581 → 519 µs |
| 同、veil は `session_resumption = "cache"` | 9,126 | 11,076 | **1.21×** | 581 → 247 µs |

- スイートの HTTP/1.1 の負荷（`williamyeh/wrk`）は、再開した接続で新しいチケットを受けると、次の接続ではそのセッションで再開しない。
  veil（rustls）は Go の crypto/tls と同じく TLS 1.2 の再開のたびにチケットを送り直すので、この wrk は 1 回おきに完全な
  ハンドシェイクになり、wrk の CPU（4 スレッド）が律速になる（veil の CPU / 要求は nginx より少ない）。TLS 1.2 を session ID で
  再開する `"cache"` なら毎回再開する（ガイドの「TLS Session Resumption」の注記）。現行のクライアントでは既定のままで veil が上回る。
- 接続を閉じる前の close_notify（B-401）は、応答を EOF まで読む OpenSSL 3 のクライアントの再開率を 0% から 99% にした（手元の
  Python。wrk は EOF を読まないので上の数字は変わらない。lab60 で B-401 の前 4,255・後 4,066）。

---

## 要求との対応

外部の値は HttpArena への提出（v0.10.1）のもの。結果は aofish（下の「計測条件」）の 8 CPU で、v0.11.0 → v0.12.0 の交互 A/B の中央値。

| 領域 | 外部の値 | 目標 | 結果 |
|---|---|---|---|
| JSON + 圧縮（json-comp、[F-296](../backlog/features/F-296-json-comp-throughput.md)・[B-331](../backlog/bugs/B-331-brotli-huge-mapping-per-request.md)） | 36.7k req/s（CPU 約 900%） | 最低でも数倍、理想は数十万 req/s | `gzip, br` で epoll 12,978 → 58,703（**4.52×**）、io_uring 13,104 → 83,167（**6.35×**）。p99 26〜27 ms → 5〜7 ms |
| コア制限下の遅延（latency-500k-8cpu、[F-297](../backlog/features/F-297-core-limited-latency.md)・[B-332](../backlog/bugs/B-332-reactor-hot-connection-starves-others.md)） | 達成率 0.7455、p99 数百万 µs | 8 CPU で 500k を維持し p99 を大きく下げる | 500k 固定で達成率 0.76 → **1.00**・p99 1.43 s → 22 ms（epoll）、0.69 → **1.00**・1.89 s → 4.2 ms（io_uring） |
| gRPC（unary、[F-298](../backlog/features/F-298-grpc-unary-fast-path.md)・[B-333](../backlog/bugs/B-333-h2-trailers-flush-per-stream.md)） | p99 が悪い | スループットを落とさず p99・p99.9 を下げる | 256 接続 × 100 ストリームで 318k → 1.60M・p99.9 1.95 s → 26 ms（epoll）、186k → 1.66M・283 → 28 ms（io_uring） |
| Python（2 スレッド、`offload=True`、[F-299](../backlog/features/F-299-python-gil-lane.md)・[B-334](../backlog/bugs/B-334-python-thread-state-per-call.md)・[F-302](../backlog/features/F-302-access-log-hot-path.md)） | 25.5k plaintext / 22.6k json | 50k 以上、できれば 70k（1 プロセス） | 21.5k → **58.4k** / 20.7k → **57.2k**、`async def` は 70.7k / 69.8k（`api_ab.py`）。`run_bench.py` では 69k → 207k / 71k → 169k。目標値の機械（i5-3320M、`veil-load`）では 22.9k → 48.4k / 20.4k → 46.1k |
| io_uring（計測の途中で発見、[F-300](../backlog/features/F-300-iouring-batched-submission.md)） | 既定の Linux ビルドの CPU/要求が epoll より 4 割多い | API の 3 領域（JSON + 圧縮・遅延・gRPC。Python の wheel は Linux でも epoll）で epoll と同等以上 | 3 領域の 15 行のうち 14 行で同等以上（gRPC の 256 接続 × 1 ストリームが 0.88）。下の「io_uring と epoll」 |

- **JSON + 圧縮の「数十万」は 8 CPU では届かない。** `gzip, br` の 1 要求あたりの CPU は 95 µs（io_uring）で、うち 63 µs（3 分の 2）が
  gzip の圧縮（`gzip, br` には既定の順で gzip を選ぶ。無圧縮は 32.5 µs）。
  外部の計測（64 論理 CPU）は、B-331 で 8 MiB の `mmap` とその錠の競合が消えた分だけ CPU の数に比例して伸びるはずだが、外部の再計測はしていない。
- **Python の目標値の 25.5k / 22.6k は、別の機械（i5-3320M、2 コア 4 スレッド）で `run_bench.py`（`wrk`、アクセスログ無し）で測った値。**
  その機械には `wrk` が無いので、同じ配置（サーバ CPU 0-1・負荷 CPU 2-3、2 スレッド、64 接続）を `veil-load` で測ると 22,850 → 48,417（2.12×）/
  20,373 → 46,069（2.26×）。目標値から 50k に要る倍率（1.96× / 2.21×）は超えたが、`veil-load` の値そのものは 50k に届かない（下の
  「i5-3320M（目標値の機械）」）。aofish では改修前が 2 万前後になる `api_ab.py` で 50k を超え、`run_bench.py` では改修前の 2.4〜3.0 倍。
  `offload=True` は既定のまま、マルチプロセスは使わない。
- データプレーンは Rust のまま（io_uring・epoll・kTLS・rustls）。新しい io_uring のオペコードは足していない。

---

## API の経路（`api_ab.py`、2026-11-07〜08）

- 比較: v0.11.0（タグ `43c8045a`）と v0.12.0 の候補（json・latency・grpc は `4fbe17a3`、python は `03cb80d0`）。どちらの木にも同じ
  `tools/perf/api-app`（HttpArena の `veil-api` と同じ形のハンドラ）を置いてビルドした。
- 5 ラウンドの交互 A/B の中央値（Python 3.14 だけ 3 ラウンド）。**全計測でエラー・Non-2xx 0**。
- `4fbe17a3` の後の変更（F-302 のアクセスログ、B-340・B-341、B-343・B-344 のログの出力）は、アクセスログを有効にした経路と
  ログの出力だけを変える。F-302 は自身の A/B（チケット）とこの表の Python の行（`03cb80d0`）、全体は Docker のフルスイート（`df6b5aae`）で確かめた。

### JSON + 圧縮（`--scenario json`、256 接続、`/json/{count}`）

| 行 | epoll（v0.11.0 → v0.12.0） | io_uring（v0.11.0 → v0.12.0） | CPU/要求（epoll・io_uring） |
|---|---|---|---|
| 無圧縮 | 151,002 → 188,949（1.25×） | 118,747 → 242,594（2.04×） | 52.0 → 41.6・66.3 → 32.5 µs |
| gzip | 61,925 → 62,706（1.01×） | 61,967 → 81,632（1.32×） | 127.7 → 125.4・127.7 → 97.6 µs |
| `gzip, br` | 12,978 → **58,703**（4.52×）、p99 26.4 → 7.1 ms | 13,104 → **83,167**（6.35×）、p99 27.4 → 4.8 ms | 294.9 → 134.3・286.7 → 95.3 µs |

br は要求ごとの 8 MiB のリングバッファの `mmap`・`munmap` をやめ、全量を 1 回で圧縮する（B-331・F-296）。io_uring の伸びが
epoll より大きいのは F-300（提出をまとめる）と F-301（3KB を超える本文をヘッダと 1 回で書く）の分。

### コア制限下の遅延（`--scenario latency`、1024 接続、`/baseline11?a=1&b=2`）

固定レートの行は予定の時刻から測る（Coordinated Omission を含む）。達成率は実際の req/s ÷ 予定の req/s。

| 行 | epoll（v0.11.0 → v0.12.0） | io_uring（v0.11.0 → v0.12.0） |
|---|---|---|
| 飽和の req/s | 372,007 → 578,847（1.56×） | 338,088 → 664,215（1.96×） |
| 飽和の CPU/要求 | 21.1 → 13.4 µs | 23.2 → 11.8 µs |
| 300k の p99 | 2.3 → 1.8 ms | 18.7 → 3.6 ms |
| 400k の達成率・p99 | 0.94・367 ms → 1.00・1.3 ms | 0.89・843 ms → 1.00・3.9 ms |
| 475k の達成率・p99 | 0.80・1.17 s → 1.00・6.5 ms | 0.68・1.92 s → 1.00・1.7 ms |
| 500k の達成率・p99 | 0.76・1.43 s → **1.00・22.4 ms** | 0.69・1.89 s → **1.00・4.2 ms** |

v0.11.0 の 8 CPU の飽和（epoll 372k）は外部の latency-500k-8cpu の値（372k）とほぼ同じで、この計測が外部の結果を再現している。
epoll は要求ごとの登録の上げ下げをやめ（F-297）、速い接続がワーカーを占有しないようにした（B-332）。

### gRPC（`--scenario grpc`、`veil-load --mode grpc`、unary）

| 行 | epoll（v0.11.0 → v0.12.0） | io_uring（v0.11.0 → v0.12.0） |
|---|---|---|
| 256 接続 × 100 ストリーム | 317,906 → 1,602,602（5.0×）、p99.9 1.95 s → 26.4 ms | 185,788 → 1,661,655（8.9×）、p99.9 283 → 27.9 ms |
| 64 接続 × 10 ストリーム | 356,385 → 889,739（2.5×）、p99 19.5 → 1.5 ms | 177,579 → 910,501（5.1×）、p99 8.5 → 1.2 ms |
| 256 接続 × 1 ストリーム | 213,419 → 321,887（1.51×）、p99 2.3 → 1.6 ms | 170,769 → 283,252（1.66×）、p99 2.7 → 1.6 ms |
| 固定 200k の達成率・p99 | 1.00・126 ms → 1.00・2.6 ms | 0.85・1.10 s → 1.00・1.9 ms |
| 固定 300k の達成率・p99 | 0.92・977 ms → **1.00・3.4 ms** | 0.56・2.67 s → **1.00・4.1 ms** |
| （参考）h2 の GET、256 接続 × 100 ストリーム | 1,995,971 → 2,239,649（1.12×）、最大 1.49 s → 26 ms | 992,185 → 2,408,244（2.43×）、最大 45 → 21 ms |

- 本文の揃った unary の要求は、本文のチャネル・中継のタスクを作らずに起動し、応答・トレーラを 1 回の書き込みで送る（F-298・B-333）。
- 参考の行の epoll の p50 は 4.5 → 11.5 ms に上がった。閉ループの平均の遅延は「同時の要求数 ÷ スループット」（25,600 ÷ 224 万 ≒ 11.4 ms）で決まり、
  v0.11.0 の小さな p50 は、一部の接続を 1.5 秒待たせて残りを速く回していた分（B-332 の不公平）。最大は 1.49 s → 26 ms。
- 1 要求あたりのシステムコール（epoll、256 接続 × 1 ストリーム）: `send` 1.99 → 0.95、`epoll_ctl` 2.98 → 0、`poll` 3.98 → 2.85。

### Python（`--scenario python`、サーバ CPU 0-3、64 接続、アクセスログ有効）

`tools/api-bench/python` の `veil_app.py`（`/plaintext`・`/json`）を、Python 3.12 と 3.14 の venv で v0.11.0 と v0.12.0 の wheel に
入れ替えて測った。「1 コア」はサーバを 1 つの物理コア（CPU 0-1）に固定した回。

| 行（ハンドラ 2 スレッド） | Python 3.12 | Python 3.14 | 3.12、1 コア |
|---|---|---|---|
| `offload=True`（既定）、plaintext | 21,494 → **58,404**（2.72×）、p99 8.4 → 1.9 ms | 19,163 → 60,762（3.17×） | 35,418 → 58,207（1.64×） |
| `offload=True`（既定）、json | 20,745 → **57,198**（2.76×）、p99 8.9 → 2.1 ms | 20,795 → 57,171（2.75×） | 35,561 → 57,476（1.62×） |
| `offload=False`、plaintext | 17,872 → 61,897（3.46×） | 16,842 → 64,391（3.82×） | 21,127 → 62,161（2.94×） |
| `offload=False`、json | 17,759 → 57,163（3.22×） | 16,831 → 55,631（3.31×） | 20,350 → 53,839（2.65×） |
| `async def`、plaintext | 23,110 → 70,688（3.06×） | 23,366 → 70,777（3.03×） | 40,599 → 52,438（1.29×） |
| `async def`、json | 23,746 → 69,753（2.94×） | 23,355 → 71,159（3.05×） | 38,622 → 51,284（1.33×） |

- ハンドラ 4 スレッドの `offload=True` は 3.12 で 53,950 → 99,206（plaintext）・46,439 → 92,694（json）。
- CPU/要求は `offload=True`・2 スレッドで 90.3 → 33.4 µs。GIL を握ったまま続けて処理する走者 1 本と控え 1 本の交代（F-299）、
  thread state の使い回し（B-334）、アクセスログの値を写すだけにする（F-302）の分。
- v0.11.0 は 1 コアに固定した方が速い（`offload=True` で 35.4k と 21.5k）。v0.12.0 の `offload=True`・`offload=False` は 4 CPU と 1 コアで
  ほぼ同じ（58.4k と 58.2k）。`async def` は 1 コアでは 52k に下がる（4 CPU では 70.7k）。
- `offload=False` はネットワークのワーカーがそれぞれ GIL を取ってハンドラを呼ぶので、ハンドラの走者の交代（F-299）が無い。4 スレッドにすると
  2 スレッドより遅い（3.12 の json で 57.2k → 24.3k。ライブラリのガイドに書いた）。

### Python と FastAPI（`run_bench.py --group python`）

ライブラリのガイド（[`docs/guide/library.md`](../guide/library.md)）の表と同じ回。サーバを CPU 0-1（1 物理コア）に固定し、
`wrk -t2 -c64 -d10s` を CPU 2-3 から、3 ラウンドの中央値。アクセスログは無効（FastAPI 側に合わせる）。
FastAPI 0.162 / uvicorn 0.54（uvloop・httptools）、Python 3.12。

| 対象 | plaintext | json | p99（plaintext） | CPU/要求 | RSS |
|---|---:|---:|---:|---:|---:|
| FastAPI `async def`、uvicorn 1 ワーカー | 9,916 | 7,474 | 14.16 ms | 102.8 µs | 49.0 MB |
| FastAPI `async def`、uvicorn 2 ワーカー | 21,873 | 17,201 | 6.02 ms | 92.3 µs | 133.8 MB |
| FastAPI `def`、uvicorn 2 ワーカー | 14,119 | 13,085 | 30.21 ms | 140.9 µs | 138.3 MB |
| veil `offload=True`（既定） | **206,599** | **168,852** | 0.76 ms | 9.5 µs | 36.6 MB |
| veil `offload=False` | 236,838 | 161,223 | 0.74 ms | 7.5 µs | 35.1 MB |
| veil `async def` | 153,451 | 132,653 | 1.02 ms | 11.2 µs | 36.5 MB |
| v0.11.0 `offload=True` | 69,245 | 70,802 | 2.71 ms | 27.1 µs | 33.9 MB |
| v0.11.0 `offload=False` | 26,157 | 21,047 | 5.19 ms | 65.2 µs | 32.4 MB |
| v0.11.0 `async def` | 88,731 | 92,418 | 2.41 ms | 19.8 µs | 33.9 MB |

`api_ab.py` の 1 コアの行（58k）とは、アクセスログの有無と負荷生成（`wrk` の 2 スレッドを CPU 2-3、`veil-load` の 8 スレッドを CPU 8-15）が
違う（どちらもパイプライン無しの HTTP/1.1、64 接続）。差の大部分は負荷の置き場所による。同じサーバ（v0.12.0 の `offload=True`、ログ無し）を
4 通りの負荷で交互に 3 ラウンド測った中央値（[`linux_loadgen_calib_raw.tsv`](linux_loadgen_calib_raw.tsv)）:

| 負荷 | plaintext | json |
|---|---:|---:|
| `wrk` 2 スレッド、CPU 2-3（`run_bench.py` の配置） | 235,951 | 179,179 |
| `veil-load` 2 スレッド、CPU 2-3 | 182,115（0.77×） | 169,667（0.95×） |
| `wrk` 8 スレッド、CPU 8-15 | 62,642 | 64,439 |
| `veil-load` 8 スレッド、CPU 8-15（`api_ab.py` の配置） | 62,502（1.00×） | 61,948（0.96×） |

負荷を CPU 8-15 に置くと道具によらず 62k 前後に下がる（WSL2 の中からは L3 が 1 つに見え、理由は切り分けていない）。CPU 2-3 の 2 スレッドでは
`veil-load` が `wrk` より低く出る（plaintext で 0.77 倍）。

### i5-3320M（目標値の機械、2026-11-08）

Python の目標値（25.5k / 22.6k）を `run_bench.py` で測った機械（Intel Core i5-3320M、2 コア 4 スレッド、Python 3.12、Linux 6.8）。
`run_bench.py` と同じ配置（サーバ CPU 0-1、負荷 CPU 2-3、64 接続、パイプライン無し）で、`wrk` が無いので `tools/perf/veil-load`（2 スレッド）を使い、
`veil_app.py` の wheel を v0.11.0（`43c8045a`）と v0.12.0 の候補（`c9689c23`）で入れ替えて 5 ラウンド交互に測った中央値。
生データは [`linux_python_i5_raw.tsv`](linux_python_i5_raw.tsv)。**全計測でエラー 0**。

| 行（ハンドラ 2 スレッド） | アクセスログ無し（目標値と同じ） | アクセスログ有効 | CPU/要求（ログ無し） |
|---|---|---|---|
| `offload=True`（既定）、plaintext | 22,850 → **48,417**（2.12×）、p99 7.0 → 2.3 ms | 20,556 → 45,681（2.22×） | 78.2 → 41.0 µs |
| `offload=True`（既定）、json | 20,373 → **46,069**（2.26×）、p99 8.3 → 2.5 ms | 18,833 → 43,413（2.31×） | 83.0 → 42.6 µs |
| `offload=False`、plaintext | 24,909 → 45,344（1.82×） | 19,370 → 42,997（2.22×） | 79.2 → 42.9 µs |
| `offload=False`、json | 23,215 → 41,111（1.77×） | 18,296 → 39,171（2.14×） | 84.9 → 48.0 µs |
| `async def`、plaintext | 23,967 → 27,422（1.14×） | 23,286 → 28,039（1.20×） | 75.3 → 61.9 µs |
| `async def`、json | 22,283 → 24,667（1.11×） | 22,048 → 25,257（1.15×） | 78.5 → 66.6 µs |

- **`veil-load` の値では 50k に届かない。** 目標値から 50k に要る倍率（plaintext 1.96×・json 2.21×）は超えた（2.12×・2.26×）。改修前の値は
  目標値の 0.90 倍（22,850 と 25.5k、20,373 と 22.6k）で、同じ比なら v0.12.0 は `wrk` で 54k / 51k に当たる（推定。この機械の `wrk` では測っていない）。
- サーバの 2 つの CPU は使い切っている（48.4k × 41.0 µs ≒ 2.0 CPU 秒/秒）。この機械の `veil-load` で 50k を超えるには、1 要求あたり 40 µs を
  切る必要がある。
- `async def` は 1 コアでは伸びが小さい（aofish の 1 コアでも 1.29×）。目標（`offload=True`）の外なので、この版では追っていない。
- レーンの走者が、列が空になっても GIL を握ったまま 20・50 µs 次の要求を待つ試作は、同じバイナリの待ち 0 と比べて ±2% の範囲で、
  速くならなかった（入れていない。F-299）。

---

## プロキシ本体（`proxy_ab.py`、2026-11-08）

- 比較: v0.11.0（`43c8045a`）と v0.12.0 の候補（`4fbe17a3`）の `veil` を Docker を使わずに交互に（3 ラウンド × 2 回、中央値）。
- 構成: 静的配信（3B・54KB）と上流（nginx、平文 HTTP/1.1）への中継を、平文 HTTP/1.1・TLS の HTTP/1.1・h2c・TLS の HTTP/2・HTTP/3 で。
  - 標準: サーバ CPU 0-7・8 スレッド、負荷 CPU 8-13（6 スレッド）、上流の nginx は CPU 12-15。
  - サーバ律速: サーバ CPU 0-3・4 スレッド、負荷 CPU 4-13（10 スレッド）。
  - HTTP/3: 標準の配置で `h2load`（QUIC）。
- **全計測でエラー・Non-2xx 0**（TSV の `wrk_timeouts` は WSL2 の時計の巻き戻りによる `wrk` の偽の timeout。下の「計測条件」）。

| 組 | バックエンド | 構成数 | 比の中央値 | 最小〜最大 | 0.95 未満 | CPU/要求の比の中央値 |
|---|---|---:|---:|---|---:|---:|
| 標準 | io_uring | 16 | **1.093** | 0.952〜1.291 | 0 | 0.888 |
| 標準 | epoll | 16 | **1.020** | 0.941〜1.181 | 2 | 0.976 |
| サーバ律速 | io_uring | 16 | **1.175** | 1.036〜1.507 | 0 | 0.852 |
| サーバ律速 | epoll | 16 | **1.065** | 0.929〜1.217 | 1 | 0.937 |
| HTTP/3 | io_uring | 4 | **1.111** | 1.046〜1.161 | 0 | 0.887 |
| HTTP/3 | epoll | 4 | **1.019** | 1.001〜1.134 | 0 | 0.983 |

- 0.95 を下回ったのは epoll の静的配信の 54KB の h2c（標準 0.941・サーバ律速 0.929）と h2（標準 0.946）だけ。B-332 で速い接続が
  ワーカーを占有しないようにした（1 回の poll の予算と poller を見る間隔）公平の代価で、予算・間隔を外したときだけ戻る。
  外すと 1 つの接続が温かい作業集合のまま続けて書けるのでキャッシュの局所性が上がるが、gRPC の p99.9 が秒単位に戻る。
  3B の行・HTTP/1.1 の行と、io_uring（完了の順で公平）には出ない（[F-297](../backlog/features/F-297-core-limited-latency.md) の「公平の代価」）。
- 大きく伸びたのは io_uring の静的配信の 3B（HTTP/1.1 1.20〜1.51、h2・h2c 1.25〜1.33）。F-300 で SQE を park でまとめて提出し、
  HTTP/2 のストリームの初回 poll をその場で行うようにした分。

### io_uring と epoll

各版の io_uring ÷ epoll の rps。バックエンドごとに別の回で測った値の比で、数 % の揺れを含む。

| 対象 | v0.11.0 | v0.12.0 | 0.95 未満の行（v0.11.0 → v0.12.0） |
|---|---|---|---:|
| API の 3 領域（JSON・遅延・gRPC、15 行） | 0.50〜1.01（1.00 以上は 4 行。うち 2 行は両方とも達成率 1.00 の固定レート） | 0.88〜1.42（1.00 以上は 14 行。うち固定レートの 6 行は両方とも達成率 1.00） | 11 → 1 |
| プロキシ・標準（16 構成） | 中央値 0.90、0.64〜1.11 | 中央値 0.98、0.74〜1.18 | 11 → 6 |
| プロキシ・サーバ律速（16 構成） | 中央値 0.96、0.57〜1.23 | 中央値 1.04、0.71〜1.34 | 7 → 3 |
| プロキシ・HTTP/3（4 構成） | 中央値 0.83、0.69〜0.94 | 中央値 0.89、0.74〜0.97 | 4 → 3 |

Python の wheel は Linux でも epoll のビルドなので、この比には入らない。

残り（v0.12.0 の要求の外。原因は確かめていない）:

- **HTTP/2。** 静的配信の 54KB の h2c（標準 0.74・サーバ律速 0.71）・3B の h2（0.90・0.79）・3B の h2c（標準 0.94）、中継の 54KB の h2c（標準 0.85）・
  3B の h2（標準 0.948）。
  54KB の行の見立て: HTTP/2 の送出はどちらのバックエンドも `write_buf` を 1 回で書くが、io_uring は送信の完了まで接続のタスクが待つ。
  10 ストリーム × 54KB は 128KB ごとのフラッシュに分かれ、io_uring ではフラッシュごとに park を 1 周回する。送信中も次のフレームを組み立てる
  二重のバッファが要る。
- **HTTP/3。** 54KB の静的配信 0.74・中継 0.88、3B の静的配信 0.90。UDP の送出の経路（io_uring は `SENDMSG` の SQE、reactor は `sendmmsg(2)`）が違う。
- **HTTP/1.1 の静的配信の 3B。** 平文の標準 0.91、TLS のサーバ律速 0.92（同じ 3B の平文のサーバ律速は 1.34）。
- **gRPC の 256 接続 × 1 ストリーム（0.88）。** CPU/要求が 27.9 µs と 24.5 µs。HTTP/1.1 の同じ形（1024 接続 × 1 要求）は io_uring が 1.15 倍速い。

io_uring の v0.11.0 → v0.12.0 は、プロキシの 36 構成のうち 35 で同じか速い（下限は標準の h2c の中継の 54KB の 0.952。3 ラウンドとも
v0.11.0 より 2〜6% 遅く、CPU/要求は 82 → 86 µs。同じ構成のサーバ律速では 1.12 倍）。

---

## Linux x86_64 のフルスイート（Docker、`suite_ab.sh`）

2026-11-10、aofish の Docker Desktop、コンテナ間（計測条件は下の「計測条件」）。

- 比較:
  - v0.12.0 の最終候補（`feat/v0120-perf` の `df6b5aae`）、イメージは `veil:v0120-uring`
  - v0.11.0 の最終候補（`325e92c3`。タグ `43c8045a` との `src/` の差は L4 のピン留めの共通化・macOS のログのレベル・mesh-linkerd の修正で、ここで測る経路は同じ）、
    イメージは `veil:v0110-rc`
  - `nginx:alpine`
- イメージ: どちらも `cargo build --profile dist --features full`（io_uring）を、同じ実行イメージに `/veil` だけ差し替えて入れた。
- 実行: `BASE=v0110-rc NEW=v0120-uring NGINX=1 bash tools/perf/suite_ab.sh`（69 構成 × 3 反復、構成ごとに順を入れ替えて交互、約 2 時間）。
- wrk の rps は要求数 ÷ 設定の秒数で求めている（[B-357](../backlog/bugs/B-357-wrk-rps-wall-clock-jump.md)）。aofish の Docker の VM は壁時計が戻るので、同じボリュームに残っていた wrk のログ 766 本のうち 193 本が
  10 秒の実行を 9.5 秒未満（最短 7.3 秒）と報告している（wrk の `Requests/sec` のままだと、その行は最大 1.37 倍に見える）。
- **全 834 計測で Non-2xx = 0、NA 行ゼロ。**

### v0.11.0 との退行確認（交互 A/B）

| プロトコル | 組数 | 比の中央値 | 最小 | 最大 | 0.95 未満 |
|---|---|---|---|---|---|
| HTTP/1.1 | 54 | 1.015 | 0.889 | 1.112 | 5 |
| HTTP/2 | 45 | 1.042 | 0.932 | 1.195 | 3 |
| HTTP/3 | 19 | 1.038 | 0.933 | 1.147 | 1 |
| h2c | 2 | 1.034 | 1.022 | 1.047 | 0 |
| gRPC | 6 | 1.067 | 1.055 | 1.227 | 0 |
| gRPC over HTTP/3 | 6 | 1.060 | 0.984 | 1.165 | 0 |
| WebSocket | 1 | 0.922 | — | — | 1 |
| **全体** | 133 | **1.029** | 0.889 | 1.227 | 10 |

0.95 を下回った 10 組の 9 構成は、1 反復ずつ順を入れ替えて 5 回測り直した（`linux_remeasure_raw.tsv`、18 組）。

| プロトコル | 組数 | 比の中央値 | 最小 | 最大 | 0.95 未満 |
|---|---|---|---|---|---|
| HTTP/1.1 | 8 | 1.022 | 0.901 | 1.081 | 2 |
| HTTP/2 | 7 | 0.992 | 0.968 | 1.075 | 0 |
| HTTP/3 | 2 | 1.070 | 1.064 | 1.075 | 0 |
| WebSocket | 1 | 1.027 | — | — | 0 |
| **全体** | 18 | **1.023** | 0.901 | 1.081 | 2 |

このスイートの比は 1 回ごとに大きく揺れる（例: `h2_1_proxy_otel` の HTTP/2 のラウンドごとの比は 0.822・1.053・1.300・1.199・0.996）。100 接続のコンテナ間では veil は CPU を使い切らず、
rps は負荷生成と仮想 CPU の起床の待ちで決まる。

測り直しで 0.95 未満に残ったのは、どちらもアクセスログ有り・ファイルキャッシュ無しの HTTP/1.1（TLS）の 2 組だけ（`h2_1_feat_access_log` 0.901、`h3_file_access_log` 0.916。
同じ構成の HTTP/2・HTTP/3 は 0.974〜1.064）。要求あたりの費用は下がっていて、rps だけが下がっている。

| `h2_1_feat_access_log`（5 ラウンド） | v0.11.0 | v0.12.0 |
|---|---|---|
| rps | 29,097 | 26,215 |
| コンテナの CPU | 580% | 446% |
| 1,000 rps あたりの CPU | 19.95% | 17.00%（−15%） |

- `h3_file_access_log` も同じ形（rps 29,482 → 27,008、CPU 579% → 449%、1,000 rps あたり 19.64% → 16.64%）。p99 は 3 反復のスイートで 10.88 → 9.26 ms と 9.31 → 8.46 ms。
- v0.11.0 だけ、ログ有りの行（スイートで 28,196・30,632）がログ無しの同じ形の行（`h2_1_feat_metrics` 26,552・`h2_1_feat_admin` 26,894・`h3_file_metrics` 25,679）より速い。
  v0.12.0 はログ有り（25,909・27,246）とログ無し（27,319・27,420・26,070）が同じ水準。v0.11.0 は要求ごとに futex でログのスレッドを起こしていて（[B-335](../backlog/bugs/B-335-access-log-writer-wake-per-request.md)）、
  その起床が仮想 CPU を眠らせない効果で rps が上がっている可能性があるが、これは確かめていない。
- Docker を使わない同じ経路の交互 A/B（`proxy_ab.py`、ファイルキャッシュ無し、`wrk_timeouts` が 0 の行の中央値、[`linux_native_log_ktls_raw.tsv`](linux_native_log_ktls_raw.tsv)。全行エラー 0）では
  v0.12.0 が速く、ログ有りの退行は無い。

| 条件（アクセスログ有り、new/base の rps の比） | HTTP/1.1 3B | HTTP/1.1 + TLS 3B | HTTP/1.1 54KB | HTTP/1.1 + TLS 54KB |
|---|---|---|---|---|
| 4 ワーカー（サーバ律速、CPU/要求 µs） | 1.187（40.33 → 32.14） | 1.315（40.97 → 31.65） | 1.362（42.83 → 31.69） | 1.282（72.54 → 55.56） |
| 8 ワーカー | 1.744 | 1.379 | 1.569 | 1.171 |
| 8 ワーカーの「ログ有り ÷ ログ無し」の rps（v0.11.0 → v0.12.0） | 0.66 → 1.01 | 0.76 → 1.02 | 0.68 → 1.03 | 0.91 → 0.98 |

スイートと同じ形（16 ワーカーで全 CPU を負荷と共有、`wrk -t4 -c100`、TLS + 54KB、ログ有り、9 ラウンド）でも 44,844 → 48,939（1.091、CPU/要求 129.17 → 112.59 µs）。
kTLS の有無でも同じ（`h1tls_file` の kTLS 有り、ファイルキャッシュ有り: サーバ律速 3B 1.304・54KB 1.117、8 ワーカー 3B 1.100・54KB 1.072）。

### nginx との比較（同じ仕事の相手、Req/s 中央値）

対応は `tools/perf/vs_nginx.py` の表のとおり（中継は `base_proxy`、圧縮は `base_compression`、中継 + 圧縮は `base_proxy_compression`、それ以外は `base`）。

| プロトコル | 構成数 | 対 nginx の中央値 | 最小 | 最大 | nginx 未満 |
|---|---|---|---|---|---|
| HTTP/1.1 | 54 | 0.75 | 0.41 | 2.94 | 32 |
| HTTP/2 | 45 | **1.47** | 0.96 | 3.57 | 1 |
| HTTP/3 | 19 | 1.00 | 0.93 | 1.06 | 9 |
| h2c | 2 | **1.74** | 1.59 | 1.90 | 0 |

代表的な構成:

| 構成 | プロトコル | nginx | veil | 対 nginx |
|---|---|---|---|---|
| `h2_1_ktls_0_lb_cbpf_ofc_1`（静的） | HTTP/2 | 30,262 | 42,688 | **1.41×** |
| `h2_1_feat_proxy`（中継） | HTTP/2 | 19,956 | 39,341 | **1.97×** |
| `h2_1_proxy_compression`（中継 + 圧縮） | HTTP/2 | 7,503 | 23,484 | **3.13×** |
| `h2_1_proxy_compression`（中継 + 圧縮） | HTTP/1.1 | 8,652 | 25,001 | **2.89×** |
| `h2_1_feat_compression`（毎回圧縮） | HTTP/1.1 | 10,506 | 26,600 | **2.53×** |
| `h3_proxy_cache`（中継 + キャッシュ） | HTTP/1.1 | 37,346 | 50,393 | **1.35×** |
| `h2_0_feat_l4`（L4 平文） | HTTP/1.1 | 57,529 | 65,399 | **1.14×** |
| `h2c_file` / `h2c_proxy` | h2c | 65,984 / 42,775 | 125,317 / 67,926 | **1.90× / 1.59×** |
| `h2_1_ktls_0_lb_kernel_ofc_1`（静的） | HTTP/1.1 | 57,529 | 43,491 | 0.76× |
| `h2_0_ktls_0_lb_cbpf_ofc_0`（静的、ファイルキャッシュ無し） | HTTP/1.1 | 57,529 | 27,414 | 0.48× |
| `h3_file_metrics`（静的） | HTTP/3 | 21,856 | 20,589 | 0.94× |
| `h3_proxy`（中継） | HTTP/3 | 18,136 | 18,997 | 1.05× |

nginx の HTTP/1.1 の基準（`base`）は反復ごとに 53,635・57,529・58,302 と揺れている（中央値の 57,529 を使った）。HTTP/1.1 の比の読み方は v0.11.0 の
切り分けと同じで、`wrk -t4 -c100` のこのスイートは負荷生成の側で決まる行が多い。nginx の `base`（443）は `open_file_cache` を使っていない
（nginx の `open_file_cache` は h2c の 8080 だけ）ので、`*_ofc_0`（veil もファイルキャッシュ無し）が `base` と同じ条件、`*_ofc_1` は veil だけが
キャッシュを使う条件になる。v0.12.0 ではこの切り分けの計測はやり直していない。サーバが律速の条件での比較は上の「プロキシ本体」。

---

## FreeBSD 14.3 aarch64（2026-11-08、QEMU + HVF、ネイティブ）

- コミット: `c9689c23`（v0.12.0 の候補。最終候補と `src/` は同じ）。VM のメモリ 4GB・4 コア。
- 実行: v0.11.0 との交互 A/B は `tools/perf/bsd/version_ab_bsd.sh`（4 ラウンド、8 シナリオ × 3B・54KB）、対 nginx は
  `tools/perf/bsd/run_perf_bsd.sh -r 4 -d 10`（54KB と `-p /small.html` の 3B。サーバ律速は `SRV_CPUS=0 GEN_CPUS=1,2,3 UP_CPUS=1,2,3
  SRV_THREADS=1 LOAD_THREADS=3`）。
- **全計測でエラー 0**

### v0.11.0 との退行確認（交互 A/B）

同じ VM で v0.11.0（`43c8045a`）と v0.12.0 の候補（どちらも `--features full-freebsd` の release）を交互に 4 ラウンド。比は v0.12.0 / v0.11.0 の
中央値（rps は大きいほど、CPU/要求は小さいほど良い）。

| シナリオ | 3B rps | 3B CPU/要求 | 54KB rps | 54KB CPU/要求 |
|---|---|---|---|---|
| `h1_file_plain` | 1.033 | 0.938 | 0.999 | 0.983 |
| `h1_file_tls` | 0.973 | 1.002 | 0.999 | 0.992 |
| `h1_proxy_tls` | 1.063 | 0.940 | 1.007 | 0.999 |
| `h2_file_tls` | **1.174** | **0.850** | 0.986 | 0.990 |
| `h2_proxy_tls` | 1.061 | 0.938 | 0.983 | 1.012 |
| `h2c_file_plain` | **1.203** | **0.831** | 1.028 | 0.897 |
| `h3_file` | 0.997 | 1.002 | 1.045 | 0.956 |
| `l4_tcp` | 1.027 | 0.972 | 1.025 | 0.981 |

**退行なし。** HTTP/2・h2c の 3B は 17〜20% 速く、CPU/要求は 15〜17% 少ない（変更ごとの切り分けはしていない）。
`h1_file_tls` 3B の 0.973 は CPU/要求が同じ（1.002）で、VM の揺れの範囲。

### 対 nginx

「標準」は計測対象 2 コア・負荷 2 コア、「サーバ律速」は計測対象 1 コア・負荷 3 コアの rps の比（4 反復の中央値）。

| シナリオ | 大きさ | nginx（標準） | veil（標準） | 標準 | サーバ律速 |
|---|---|---|---|---|---|
| `h1_file_tls`（HTTPS 静的） | 54KB | 28,934 | 38,111 | **1.32** | **1.49** |
| `h2_file_tls`（HTTP/2 静的） | 54KB | 26,243 | 37,950 | **1.45** | **1.54** |
| `h1_file_plain`（平文 HTTP/1.1 静的） | 54KB | 62,357 | 63,227 | 1.01 | 0.96 |
| `h2c_file_plain`（h2c 静的） | 54KB | 83,774 | 71,677 | 0.86 | **1.19** |
| `h3_file`（HTTP/3 静的） | 54KB | 4,128 | 3,891 | 0.94 | 0.98 |
| `h1_proxy_tls`（HTTPS 中継） | 54KB | 19,648 | 21,799 | **1.11** | **1.12** |
| `h2_proxy_tls`（HTTP/2 中継） | 54KB | 15,825 | 20,066 | **1.27** | **1.40** |
| `l4_tcp`（L4 TCP） | 54KB | 44,554 | 50,087 | **1.12** | **1.12** |
| `h1_file_tls` | 3B | 282,482 | 265,388 | 0.94 | **1.08** |
| `h2_file_tls` | 3B | 192,303 | 697,348 | **3.63** | **1.22** |
| `h1_file_plain` | 3B | 359,866 | 385,177 | **1.07** | **1.16** |
| `h2c_file_plain` | 3B | 132,025 | 786,363 | **5.96** | **1.65** |
| `h3_file` | 3B | 158,120 | 170,547 | **1.08** | 0.98 |
| `h1_proxy_tls` | 3B | 160,096 | 147,680 | 0.92 | 1.04 |
| `h2_proxy_tls` | 3B | 44,354 | 86,675 | **1.95** | **1.61** |
| `l4_tcp` | 3B | 219,206 | 210,890 | 0.96 | 0.90 |

- VM の rps はホストの負荷で大きく揺れる（同じバイナリでもラウンドごとに 3 割動く。nginx の `h2_file_tls` 3B の標準は v0.11.0 の回の
  318k に対して今回 192k）。0.90〜1.05 の行は誤差の範囲で、版の間の比較は上の交互 A/B で行う。
- `h2c_file_plain` 54KB の標準 0.86 は負荷生成が律速（サーバ律速では 1.19）。
- `h3_file` 54KB は、FreeBSD の quiche（`boringssl-boring-crate`）の経路で、本文をパケットへ詰めるコピーと quiche の内部の確保が
  上位（v0.8.0 から同じ残り）。
- この計測の後に直した静的配信のキャッシュの不具合（[B-348](../backlog/bugs/B-348-fast-path-drops-open-file-cache-fd.md)・
  [B-349](../backlog/bugs/B-349-content-cache-version-mismatch.md)）は、キャッシュに当たる経路を変えない（当たったときに増えるのは
  版の比較 2 つ）。静的配信の構成の交互 A/B で確かめた（[platform_verification.md](platform_verification.md)）。

---

## 計測条件

### API の経路・プロキシ本体（`api_ab.py`・`proxy_ab.py`・`run_bench.py`）

- aofish の WSL2（AMD Ryzen AI Z2 Extreme、8 コア 16 スレッド、カーネル 6.6.87.2、Hyper-V の下）。Docker は使わず、ループバックで測る。
- CPU は `lscpu -e` の CORE 列で兄弟（同じ物理コアの 2 スレッド）が隣り合う（0-1 がコア 0）。サーバと負荷を物理コアで分ける。
- `api_ab.py`: サーバ CPU 0-7（8 スレッド）・負荷 CPU 8-15。Python はサーバ CPU 0-3、1 コアの回は 0-1。負荷は `tools/perf/veil-load`
  （閉ループと固定レート。固定レートは全接続を順に使い、次の予定まで眠る）。
- `proxy_ab.py`: 上の「プロキシ本体」の配置。HTTP/1.1 は `wrk`、HTTP/2・h2c は `h2load`、HTTP/3 は QUIC 対応の `h2load`。
- 計測中はほかのプロセス（別の作業の計測・ビルド）を止める（WSL の `/tmp/vp-bench.lock`）。
- i5-3320M（目標値の機械）: サーバ CPU 0-1（1 物理コア）、`veil-load` の 2 スレッドを CPU 2-3。計測中はビルドを止める。
- wrk の `timeout` は `gettimeofday` の差で判定するので、WSL2 の時計が戻る（30 秒ごとに約 75 ms）とその瞬間に処理中の全接続を数える。
  1 回の実行で 100 件前後の偽の timeout になり、最大の遅延は動かない。TSV の `wrk_timeouts` に分けてエラーに数えない。

### Linux のフルスイート（Docker、`suite_ab.sh`）

- Docker のコンテナ間通信（aofish の Docker Desktop / WSL2、16 vCPU を veil・nginx・上流・負荷生成で共有）。kTLS は直交表の `ktls` 因子でだけ有効。
- 負荷: HTTP/1.1 は `wrk -t4 -c100 -d10s`（`williamyeh/wrk`。Alpine 3.8 + LibreSSL 2.7、TLS 1.2）、HTTP/2・HTTP/3 は
  `h2load -t4 -c100 -m10`、gRPC・WebSocket は k6。接続の張り直しは `wrk -t4 -c100 -d14s -H "Connection: close"` を
  `williamyeh/wrk` と `local/wrk-openssl`（Debian の wrk、OpenSSL 3、TLS 1.3。`tools/perf/wrk-openssl`）で。
- 配信: `/`（54,576B の index.html）。上流は nginx（平文 HTTP/1.1）。
- 計測中はほかの作業を止める（WSL の `/tmp/vp-bench.lock`）。

### FreeBSD（`tools/perf/bsd/`）

- ゲストの中のループバック。計測対象は 0,1 コア（サーバ律速は 0）、負荷生成と上流（別の nginx）は 2,3 コア（同 1,2,3）
- 負荷: HTTP/1.1 は `wrk -t2 -c64`、HTTP/2・h2c は `h2load -t2 -c64 -m32`、HTTP/3 は `h3load -t2 -c64 -m32`（quinn + h3）
- 静的配信は `[static_file_cache]` + `open_file_cache`（nginx も `open_file_cache`）
- v0.11.0 との交互 A/B（`version_ab_bsd.sh`）は 1 シナリオ 8 秒で、ラウンドごとに 2 つのビルドの順を入れ替える。CPU/要求は計測対象の
  プロセスの CPU 時間（`ps -o time`）÷ 要求数
- 手で A/B するときは、バイナリの名前を `veil` のままにする（`pkill -x veil` で止まらない名前にすると、`SO_REUSEPORT` で
  前のプロセスが同じポートで受け続け、計測が混ざる）

## 再現手順

### API の経路・プロキシ本体（Docker を使わない）

```bash
# 比べる版を git worktree で取り出し、api-app・veil-load は同じソースを両方の木に置いてビルドする（tools/perf/README.md）
git worktree add ../veil-base v0.11.0
cp -r tools/perf/api-app tools/perf/api_ab.py ../veil-base/tools/perf/
(cd tools/perf/veil-load && cargo build --release)
(cd tools/perf/api-app && cargo build --release)                 # io_uring（--features epoll で epoll）
(cd ../veil-base/tools/perf/api-app && cargo build --release)
python3 tools/perf/api_ab.py --new . --base ../veil-base --scenario json
python3 tools/perf/api_ab.py --new . --base ../veil-base --scenario latency --rates 300000,400000,475000,500000 --rounds 5
python3 tools/perf/api_ab.py --new . --base ../veil-base --scenario grpc
python3 tools/perf/api_ab.py --new . --base ../veil-base --new-venv <新の venv> --base-venv <旧の venv> --scenario python

# プロキシ本体（両方の veil を cargo build --release --features full（,epoll）でビルドしておく）
python3 tools/perf/proxy_ab.py --base <旧の veil> --new <新の veil> --rounds 3 --duration 4 \
  --cases '^(h1|h1tls|h2c|h2)_(proxy|file)_'
python3 tools/perf/proxy_ab.py --base <旧の veil> --new <新の veil> --rounds 3 --duration 4 \
  --server-cpus 0-3 --threads 4 --load-cpus 4-13 --load-threads 10 --proxy-load-cpus 4-11 --proxy-load-threads 8 \
  --cases '^(h1|h1tls|h2c|h2)_(proxy|file)_'
LD_LIBRARY_PATH=<h2load-h3>/lib python3 tools/perf/proxy_ab.py --base <旧の veil> --new <新の veil> --rounds 3 --duration 4 \
  --h3load <h2load-h3>/h2load --cases '^h3_'

# Python と FastAPI（tools/api-bench/README.md）
WRK_BIN=/usr/bin/wrk python3 tools/api-bench/run_bench.py --group python --rounds 3

# 目標値の機械（2 コア 4 スレッド）。wrk の代わりに veil-load を run_bench.py と同じ配置で（--no-access-log の有無で 2 回）
python3 tools/perf/api_ab.py --new . --base ../veil-base --new-venv <新の venv> --base-venv <旧の venv> --scenario python \
  --server-cpus 0,1 --load-cpus 2,3 --load-threads 2 --conns 64 --py-variants offload,inline,async --py-threads 2 \
  --rounds 5 --no-access-log
```

負荷生成の較正は、`veil_app.py`（`BENCH_ACCESS_LOG=0`）を CPU 0-1 で 1 回起動し、`wrk -t2`・`veil-load -t 2`（CPU 2-3）と
`wrk -t8`・`veil-load -t 8`（CPU 8-15）を 64 接続・10 秒ずつ順に当てる（ラウンドごとに順を逆にする）。

### Linux のフルスイート（Docker）

```bash
cargo build --profile dist --features full     # 新しい側（ベースは git worktree で v0.11.0 を同じようにビルド）
# どちらも cgr.dev/chainguard/glibc-dynamic に /veil だけを入れたイメージにする（veil:new・veil:base）
bash tools/perf/gen_configs.sh
BASE=base NEW=new NGINX=1 bash tools/perf/suite_ab.sh
python3 tools/perf/suite_ab_summary.py tools/perf/results/suite_ab/<日時>.tsv base new   # 対 v0.11.0
python3 tools/perf/vs_nginx.py tools/perf/results/suite_ab/<日時>.tsv veil_new           # 対 nginx
# 測り直し（1 反復ずつ BASE と NEW を入れ替えて 5 回。1 回の中では BASE → NEW の順に測るので、入れ替えないと順序が片側に乗る）
CONFIG_GLOB=<構成> NGINX=0 ITERATIONS=1 BASE=base NEW=new bash tools/perf/suite_ab.sh
CONFIG_GLOB=<構成> NGINX=0 ITERATIONS=1 BASE=new NEW=base bash tools/perf/suite_ab.sh
```

### FreeBSD（QEMU VM）

```bash
tools/qemu/bsd-vm.sh freebsd aarch64 up
tools/qemu/bsd-vm.sh freebsd aarch64 build
# ゲスト内（pkg install -y nginx nghttp2 wrk-luajit curl、h3load をビルド）:
cargo build --release --manifest-path tools/perf/h3load/Cargo.toml
sh tools/perf/bsd/run_perf_bsd.sh -r 4 -d 10                        # 54KB
sh tools/perf/bsd/run_perf_bsd.sh -r 4 -d 10 -p /small.html         # 3B
SRV_CPUS=0 GEN_CPUS=1,2,3 UP_CPUS=1,2,3 SRV_THREADS=1 LOAD_THREADS=3 \
  sh tools/perf/bsd/run_perf_bsd.sh -r 4 -d 10                      # サーバ律速
# v0.11.0 との交互 A/B: v0.11.0 を別の木で同じ feature（full-freebsd）の release にビルドし、2 つを別のディレクトリに veil の名前で置く
mkdir -p /root/ab/old /root/ab/new
cp <v0.11.0 の木>/target/release/veil /root/ab/old/veil && cp target/release/veil /root/ab/new/veil
A_DIR=/root/ab/old B_DIR=/root/ab/new sh tools/perf/bsd/version_ab_bsd.sh > ab.tsv
```
