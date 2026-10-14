# F-206: Rust の API ライブラリを tokio 上の axum 以上の性能にする（API ライブラリの範囲だけで）

**状態: 未着手（設計済み 2026-10-14）**

## 背景

F-202 の後、`tools/api-bench`（`/plaintext`・`/json/{name}`、HTTP/1.1 キープアライブ、64 接続、サーバ 2 スレッド）で、
veil のネイティブハンドラは tokio 上の axum（per-core）に対して io_uring 93%（json 94%）・epoll 98%（json 98%）。
CPU は 1 要求あたり 31.7 µs（io_uring）/ 30.9 µs（epoll）対 axum 29.6 µs。axum を `tokio_service` で載せた経路は
45,765 req/s（axum + tokio の 67%、30 allocs/req）。

残りの差の候補（F-202 の strace・ALLOC_TRACE より）:

1. キープアライブで次の要求を「readiness 待ち → 読み取り」の 2 段で待つ（io_uring は `POLL_ADD` と `RECV` の 2 操作。
   B-101 でアイドル接続に読み取りバッファを持たせないようにした設計）。
2. 1 要求あたり 4 回の確保（要求ヘッダの `Vec`・`handle_api_handler` の `Box`・ハンドラの `Box<dyn Future>`・応答ヘッダの `Vec`）。
3. ルーティングがプロキシと共通（`config::load_backend` の経路）で、ハンドラだけの App でも汎用の照合を通る。

F-205（同じ 2 段の待ちをプロキシと共通の経路で直す）とは別に、**API ライブラリの範囲だけ**で axum を超える。

## 制約（今回の依頼）

- 手を入れるのは API ライブラリ部分だけ: `src/api/`、`crates/`、`tools/api-bench/`、と本体側の `cfg(feature = "handler")` の
  分岐（AGENTS.md「本体側への追加は `cfg(feature = "handler")` の分岐だけ」）。
- `--features full`（プロキシのバイナリ）は 1 行も変わらない。`library_e2e.sh nonregression` が通り、プロキシの性能は
  バイナリが同じなので測り直し不要（念のため `tools/perf` の suite_ab で確認する）。
- ホットパス規則（ブロッキング禁止・確保禁止・ゼロコピー）はそのまま。

## 設計

### 1. API 専用リスナーのキープアライブ待ちを 1 操作にする（io_uring）

> 2026-10-14 追記: F-205 が同じ方法（provided buffer ring の `RECV` 1 回・epoll の先読み）を共通の経路に設計した。
> **F-205 を先に入れた場合、この手順は不要**（計測だけ行う）。F-205 がまだのときだけ、下の `cfg(feature = "handler")` の
> 分岐で入れる（後で F-205 に置き換える）。

- 設定ロード時に「このリスナーのルートが全部 `Backend::Handler`（ハンドラだけの App）」かを求めて、`#[serde(skip)]` の
  フィールドに置く（`cfg(feature = "handler")`）。プロキシのルートが混ざる App・本体ビルドでは今と同じ経路。
- そのリスナーの平文 HTTP/1.1 接続（kTLS で受信が平文になる TLS も対象）は、`handle_requests` のキープアライブ待ちで
  `wait_next_request` → `read` の代わりに **provided buffer ring の `RECV` を 1 回** 出す:
  - 既存の `IORING_REGISTER_PBUF_RING`（F-130、`src/runtime/uring/udp_recv.rs`）を TCP 用に 1 ワーカー 1 リングで使う。
    オペコードは既存の `RECV` に `IOSQE_BUFFER_SELECT` を付けるだけ（新しいオペコードは増やさない）。
  - アイドル接続はバッファを持たない（B-101 の性質を保つ）。リングのバッファ数 × サイズはワーカーごとの固定費
    （例: 4KB × 256 = 1MB / ワーカー。`[api]` のキーで変えられるようにする）。
  - リングが空（`ENOBUFS`）のときは今の 2 段の待ちに戻す（取りこぼさない）。
  - 受け取ったリングのバッファを要求の解析にそのまま使い、応答を書いたらリングへ返す（`Bytes` で外へ出す本文は複製しない
    ように、ハンドラへ渡す `Bytes` はリングのバッファではなく要求バッファのプールから取る。リングのバッファを長く持たない）。
- epoll（reactor）は B-75 の確認用 `poll(2)` を省略できないので変えない（今 98%。2・3 の CPU 削減で超える）。
- 分岐は `handle_requests` の待ちの 1 か所に `#[cfg(feature = "handler")]` で置く。専用の接続ループを `src/api/` に新しく
  書く案は、ヘッダ上限・タイムアウト・graceful shutdown（F-187）・アクセスログ・h2c 判別・100-continue・チャンク本文を
  二重に持つことになるので採らない。

### 2. 確保を 4 → 1 以下にする

| 確保 | 対応 |
|---|---|
| 要求ヘッダの `Vec<(Bytes, Bytes)>` | `Request` に要求ヘッダ領域の `Bytes` 1 個と位置の配列（`[[u16; 4]; N]` をインラインで持ち、超えたら `Vec`）を持たせ、`headers()` はその場で切り出すイテレータにする（`Headers` 抽出子・tower 変換のときだけ実体化）|
| `handle_api_handler` の `Box` | ハンドラ専用の経路では `handle_backend` の Future を経由せずに呼ぶ（Future の大きさの問題は 1 の分岐側で `Box` 無しに収まるかを測って決める）|
| ハンドラの `Box<dyn Future>` | 接続ごとに 1 個の再利用スロット（同じ大きさ・アラインの Future は前回の領域に置く。`CallCache` に持つ）。大きさが違うときだけ確保 |
| 応答ヘッダの `Vec` | `Response` のヘッダを 4 個までインライン（`text`・`json` の既定は `content-type` 1 個） |

`tools/api-bench/allocs.py` で `/plaintext`・`/json` の allocs/req を記録し、`ALLOC_TRACE` で残りを特定する。

### 3. ハンドラだけの App のルーティング

- ハンドラだけの App（1 と同じ判定）は、設定ロード時に `src/api/routes.rs` のパターンから
  「メソッド × 先頭セグメント」の表を作り、要求ごとの照合を表引き + パスパラメータの位置記録（確保無し）にする。
- `PathParams` は位置（`u16` の組）だけを持ち、`param()` で切り出す。

### 4. 計測を広げる

- `tools/api-bench` に HTTP/2（h2c、`h2load`）と、`#[veil::route]`（マクロ・抽出子）経由のハンドラの列を足す
  （今は `App::get` のネイティブと `tokio_service` のみ）。axum 側は hyper の HTTP/2。
- veil のネイティブハンドラの **HTTP/3** の列も足す（2026-10-14 追加）。クライアントは `h2load --npn-list h3`（tools/perf と
  同じ h3 対応ビルド）か、無ければ quinn + h3 の負荷ツール。axum には HTTP/3 が無いので比較相手は無し（veil の HTTP/2 と並べて
  記録し、退行の監視に使う）。
- `/json` の本文を書くとき `serde_json::to_vec` の `Vec` をそのまま `Bytes` にしているか（複製が無いか）を確かめる。

### 5. 載せた axum（`tokio_service`）

`http::Request` の組み立て（ヘッダの `HeaderMap` への複製）と応答の `Full<Bytes>` 化を見直す（30 allocs/req）。
受け入れ条件には入れない（目標は axum + tokio の 80%）。

## 手順

1 → 計測（io_uring だけ効くはず）、2 → 計測、3 → 計測、の順に、各段で `BENCH_BASE_DIR` の交互 A/B（両方の順番）と
allocs/req を記録する。段ごとに 1 コミット。

## 受け入れ条件

- `tools/api-bench` のネイティブハンドラが、tokio 上の axum（per-core）に対して `/plaintext`・`/json` とも
  **io_uring・epoll の両方で 100% 以上**（5 ラウンド交互、`docs/artifacts/` に TSV）。HTTP/2 の列も同等以上。
  HTTP/3 の列は記録する（比較相手なし）。
- `#[veil::route]` 経由のハンドラも同等以上。
- `library_e2e.sh` の全節・`cargo test --features api`・`tests/standalone_e2e.rs` が通る。
- `--features full` のバイナリに変化が無い（`nonregression`）。アイドル接続あたりのメモリが増えない
  （B-101 の計測を API 専用リスナーで取る）。
- `docs/guide/library.md`（英日）の Performance と `docs/perf/README.md` を更新する。

## 関連

F-201（ベンチ）、F-202（14 → 4 allocs/req）、F-205（プロキシと共通の経路の同じ問題。こちらで得た結果を持ち込む）、B-101。
