# F-226: TOML のルートの要求のタイムアウト・条件付きリトライ・ミラー・フォルトの遅延

**状態: 未着手（2026-10-16 起票・設計。F-219 の TOML 側の残り。親 F-215 の残件）**

## 目的

F-219 はメッシュのデータプレーンにルート単位の要求のタイムアウト・条件付きのリトライ・ミラー・フォルトの遅延を実装した。
メッシュ無しの TOML のルートに出したのは固定応答・重み付き分割・フォルトの中断・CORS のプリフライトだけである
（`[route.traffic]`。ルート照合の直後に Backend を選ぶだけで済むもの）。残りの 4 つを TOML のルートにも出す。

## 今の作り

- ルート照合の直後に `upstream::route_backend` が `TrafficPolicy::select` で Backend を選ぶ。HTTP/2 の plan・HTTP/3 の classify は
  乱数を引かない `find_backend_preview` を使う（`src/route_traffic.rs`）。
- 再送は F-177 の「プールの接続が応答前に切れたら、冪等で本文を受信済みの要求だけ 1 回」だけ。経路ごとに別の実装がある:
  HTTP/1.1 は `src/proxy.rs`（`handle_proxy`・`proxy_request_buffered`）、HTTP/2 の上流は `http2::upstream_mux`、
  HTTP/3 は `src/http3_stream.rs`。
- タイムアウトは接続単位だけ: `backend_connect_timeout_secs`・クライアントのヘッダと本文・アイドル・HTTP/2 のストリームのアイドル。
- メッシュの実装（`src/mesh/l7.rs` の `RetryPolicy`・`retry_on_status`、`src/mesh/mirror.rs`）は mesh の上流モジュール
  （`src/mesh/upstream.rs`）に結び付いていて、そのままでは TOML の経路で使えない。

## 設定

```toml
[[route]]
[route.traffic]
timeout_ms = 3000              # 要求全体（要求の head を受けてから応答の最後のバイトまで）。0 = 無し（既定）
[route.traffic.retry]
attempts = 2                   # 最初の 1 回を除いた再送の回数
per_try_timeout_ms = 1000      # 0 = timeout_ms だけ
retry_on = ["connect-failure", "reset", "gateway-error", "retriable-status-codes"]
retriable_status_codes = [503]
backoff_base_ms = 25           # Envoy と同じ: base から 2 倍ずつ、上限 max（既定 base × 10）、全幅のジッタ
backoff_max_ms = 250
budget_percent = 20.0          # 同時に処理中の要求に対するリトライ中の要求の割合の上限（Envoy の retry_budget）
budget_min_concurrency = 3
buffer_limit_bytes = 65536     # これを超える本文の要求はリトライしない（Envoy の per_request_buffer_limit_bytes）
[[route.traffic.mirror]]
upstream = "shadow"            # [upstreams.<name>]
percent = 10.0
max_inflight = 64              # ルートあたりのミラーの同時実行の上限。超えた分は送らずに数える
[route.traffic.fault]
delay_ms = 500                 # 既存の abort と同じ表に足す
delay_percent = 5.0
```

`retry_on` の値は Envoy と同じ名前にする（`5xx`・`gateway-error`・`reset`・`connect-failure`・`retriable-status-codes`、
gRPC の `cancelled`・`deadline-exceeded`・`resource-exhausted`・`unavailable`）。Istio・Linkerd の設定を TOML に写すときに
読み替えが要らないようにするため。

## 設計

### 共通

- ロード時に `RequestPolicy`（timeout・retry・mirror・delay）へ解決し、ルートの `#[serde(skip)]` の `Option<Arc<RequestPolicy>>` に
  持たせる。設定の無いルートは `None` を見る分岐 1 個だけで、既存の経路のコストを増やさない（F-148/F-159）。
- 判定の関数（`retry_on_status`・バックオフ・バジェット）は `src/route_traffic.rs` へ移し、メッシュと共有する。メッシュの
  `RetryPolicy` はこの型の別名にする（同じ判定を 2 つ持たない）。
- 乱数は既存のスレッドローカルの xorshift（`route_traffic` の分割と同じ）。

### 4 つの機能

- **フォルトの遅延**: 上流へ接続する前に `runtime::time::sleep`（スレッドを止めない）。HTTP/3 のバッファ経路はワーカーの
  `FuturesUnordered` の中で待つ（メインループで await しない。B-97）。
- **タイムアウト**: 上流とのやり取りの Future を、設定があるときだけ `runtime::time::timeout` で包む。io_uring でも I/O ごとに
  カーネルタイマーを張らない（ユーザ空間のヒープに 1 件足すだけ。B-72）。満了の応答:
  - 応答の head を送る前なら 504。
  - head を送った後なら接続を切る（HTTP/1.1）か、ストリームをリセットする（HTTP/2・HTTP/3）。切り詰めた本文を成功に見せない（B-86）。
  - WebSocket などのアップグレードは 101 を受けるまでだけ数える。
- **リトライ**: 本文を全部受け取り終え、`buffer_limit_bytes` 以下の要求だけを対象にする。ストリーミングの本文はリトライしない
  （Envoy と同じ）。
  - 再送用の複製は要求バッファのプールから取る（F-177 と同じ。`clone()` しない）。
  - 判定は、上流の応答の head を解析した直後、クライアントへ 1 バイトも書く前に行う。HTTP/1.1 の経路は head を解析してから
    書き出しているので、順序は変わらない。
  - F-177 の 1 回の再送は、`retry` が無いときの既定の動作として残す（挙動を変えない）。
  - HTTP/2 の上流は `upstream_mux` の新しいストリームで再送する（F-177 の経路を一般化する）。
  - バジェットはルートごとのアトミック 2 個（処理中・リトライ中）で数える。プロセス共有で、ワーカーをまたぐ（B-31 と同じ理由）。
- **ミラー**: 本文を手元に全部持っている要求だけを対象にする。要求を複製（プールから）して別タスクで送り、応答は待たずに捨てる。
  - Host には `-shadow` を付ける（Envoy と同じ）。
  - 同時実行の上限はルートごとのアトミックで数える。ストリーミングの要求と上限を超えた分は送らずにメトリクスに数える。
  - 送り先は `[upstreams.<name>]` の接続プールをそのまま使う（HTTP/1.1・h2c・TLS の設定を流用）。

### 経路ごとの入れ場所

| 経路 | 遅延・タイムアウト | リトライ | ミラー |
|---|---|---|---|
| HTTP/1.1（TLS・平文、`handle_proxy`・`proxy_request_buffered`） | ○ | バッファ済みの要求 | バッファ済みの要求 |
| HTTP/1.1 のストリーミング本文（`client_body` の転送） | ○ | × | × |
| HTTP/2（ストリームのタスク、上流は h1 プールか `upstream_mux`） | ○ | ○（`upstream_mux` は新しいストリームで） | ○ |
| HTTP/3 のバッファ経路（ワーカーの `FuturesUnordered`） | ○ | ○ | ○ |
| HTTP/3 のストリーミング経路 | ○（満了はストリームのリセット） | × | × |
| gRPC（`grpc` の Backend） | ○（`grpc-timeout` の要求ヘッダがあれば短い方） | gRPC のステータスで判定 | ○ |
| 静的配信・Respond・Handler | 対象外（上流が無い） | — | — |

### メトリクス

`veil_route_retries_total{route,reason}`・`veil_route_retry_budget_exhausted_total{route}`・`veil_route_timeouts_total{route}`・
`veil_route_mirror_total{route,result="sent|skipped|failed"}`・`veil_route_fault_delays_total{route}`。ラベルの文字列はロード時に
作る（要求ごとに `format!` しない）。

## 進め方

1 機能 1 コミットで、遅延 → タイムアウト → リトライ → ミラーの順に進める。各段で次の 2 つを確かめる。

- 設定の無いルートの非退行（`suite_ab.sh`）。
- 設定したときのコスト（`tools/perf` に `[route.traffic]` 付きの設定を足して、リトライ無しの応答で allocs/req と rps）。

## 受け入れ条件

- （必須）既存のプロキシの性能が退行しない（`tools/perf/suite_ab.sh` の main との交互 A/B、比の中央値 0.98 以上・0.95 未満なし）。
  両バックエンド（io_uring・epoll）、3B と 54KB。
- 各プロトコル（HTTP/1.1・HTTP/2・HTTP/3、gRPC）の E2E（`tests/standalone_e2e.rs`）。確かめる組み合わせ:
  - 満了の 504 と途中切断
  - 503 で再送して成功
  - POST のストリーミング本文は再送しない
  - バジェットの枯渇
  - ミラー先にだけ届き、本線の応答に影響しない（ミラー先が止まっていても遅れない）
  - 遅延の割合
- メッシュの単体・kind の試験（Istio の Request Timeouts・Mirroring・Fault Injection、Linkerd の Retries・Timeouts）が共有化の後も
  通る。
- `docs/guide/routing.md`・`load-balancing.md`（英日）・`examples/config.toml`。

## 関連

F-219、F-177、F-215、`src/route_traffic.rs`、`src/mesh/mirror.rs`、`src/mesh/l7.rs`。
