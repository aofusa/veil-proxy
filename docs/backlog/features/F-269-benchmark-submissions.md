# F-269: HttpArena（veil 単体・Rust API・Python）と the benchmarker（Python）へのベンチマークの提出

**状態: 未着手（2026-10-25 起票・計画。親は [F-258](F-258-v0100-plan.md)。F-259〜F-268 を入れた版の公開の後に着手）**

## 背景

`docs/artifacts/benchmark/task.md`（2026-10-23、v0.8.4 に固定・本体を変えない前提の提出計画）で、次の制約が分かった:

| 制約（v0.8.4） | 影響 | 解消するチケット |
|---|---|---|
| 1 プロセスに TLS 1 個・平文 1 個、ALPN はプロセスに 1 つ | HttpArena の 4 ポート（8080・8081（h2 を出さない TLS）・8082・8443 + UDP）に 2〜3 プロセス要る。待機側の CPU・メモリがコンテナ全体の計測に乗る | [F-260](F-260-multiple-listeners.md) |
| ハンドラで WebSocket を終端できない | echo-ws・echo-ws-pipeline・echo-ws-limited を B・C で購読できない | [F-261](F-261-handler-http11-upgrade.md)・[F-262](F-262-handler-websocket-rust.md)・[F-263](F-263-python-async-runtime-websocket.md) |
| Python の `async def` は同時に待てる数 = プールのスレッド数 | C の async（`/delay` を 32K 同時）が捌けない | [F-263](F-263-python-async-runtime-websocket.md) |
| ライブラリの静的配信の書き方 | B・C の static 系 | [F-267](F-267-rust-kit-crate.md) の `App::serve_dir`・[F-268](F-268-python-kit-fastapi-compat.md) の `StaticFiles` |
| Python の README が「Linux は io_uring」と誤記（wheel は epoll） | 文書の誤り | 本チケットの準備で直す（下の 1.） |

本チケットは、これらを入れた版（**v0.10.0** の予定）を公開した後に、提出物を作り直して提出する。

## 提出物

| # | 提出先 | 区分 | エントリ | 中身 |
|---|---|---|---|---|
| A | HttpArena | infrastructure | `veil` | veil のバイナリ（`full-container` = epoll）。`/baseline11`・`/baseline2`・`/pipeline`・`/json` は Proxy-Wasm のモジュール、`/static` は File バックエンド。**1 プロセス**（`[[listener]]` で 4 ポート） |
| B | HttpArena | framework（`emerging` か `experimental`、`standard`） | `veil-api` | Rust の API ライブラリ（`#[veil::route]`・`Json`・`App::serve_dir`・gRPC・WebSocket）。1 プロセス |
| C | HttpArena | framework（同上） | `veil-python` | PyPI の `veil-api`（`@veil.get` 等・`async def` は常駐のループ・`@app.websocket`）。1 プロセス |
| D | the benchmarker | Python | `python/veil` | PyPI の `veil-api`（3000 番の 3 ルート） |

作業の場所は今の計画と同じく `docs/artifacts/benchmark/`（git 管理外）。task.md は v0.10.0 用に改訂する（v0.8.4 の版は
`task-v0.8.4.md` として残す）。

## 手順

### 0. 前提の確認（公開の前に本体側で済ませること）

- v0.10.0 の crates.io（`veil-proxy`・`veil-proxy-macros`・`veil-proxy-build`・`veil-proxy-kit`）・PyPI（`veil-api`・`veil-api-kit`）・
  ghcr（`ghcr.io/aofusa/veil:0.10.0`）が公開済み。提出物は公開物だけを使う（タグからのソースビルドに頼らない）。
- 次の未確認事項（task.md の R4・R5 等）を v0.10.0 で確かめ、足りなければ **提出の前に本体のチケットを起票** する（本チケットの中で本体を直さない）:
  - ハンドラの応答に `[route.compression]`（gzip・br）が効くか（json-comp。standard は手書きの圧縮を禁止）。
  - `App::serve_dir` / `StaticFiles` のファイルの差し替えへの追従が 2 秒以内（framework の `STATIC_STALE_WINDOW`）。
  - 1 プロセス 4 リスナーの構成で、8081 が h2 を選ばない・8082 が h2c・8443 が h2 + h3。
  - Python の wheel の README の「Linux uses io_uring」の誤り（epoll）を直す（文書の修正。v0.10.0 の前に入れる）。

### 1. A（infrastructure、`veil`）

- 設定: `[[listener]]` 4 個（8080 平文 HTTP/1.1 + h2c、8081 TLS `http2 = false`、8082 h2c のみ、8443 TLS `http3 = true`）、
  `/static` の File（`open_file_cache` + `static_file_cache`、追従は 30 秒未満）、動的なエンドポイントは Proxy-Wasm（JSON は要求ごとに直列化）、
  アクセスログ無効。
- task.md の 2 章（Wasm のモジュール・R2・R3）と 5 章（複数プロセス）を見直す: 5 章は不要になる。
- `tests` は validate を通ったものだけ。WebSocket は infrastructure ではスコア対象外なので購読しない。

### 2. B（Rust、`veil-api`）

- 依存: `veil = { package = "veil-proxy", version = "=0.10.0", default-features = false, features = ["full-container", "api"] }`、
  必要なら `veil_kit`（`=0.10.0`）。`Cargo.lock` を入れる。
- エンドポイント: task.md 3.2 の表に、`/ws`（`WebSocketUpgrade`。echo-ws・echo-ws-pipeline・echo-ws-limited を購読）、
  gRPC の `GetSum`（8080 h2c・8443 TLS）、`/static`（`App::serve_dir`）を足す。リスナーはビルダの `listener(..)` で 4 個。
- `completeness` の根拠（routing・middleware（`wrap_on`・tower の `Layer`）・request・response）を README に書く。

### 3. C（Python、`veil-python`）

- `pip install veil-api==0.10.0`（abi3 wheel、Linux は epoll）。`veil.run(listeners=[...])` で 4 ポートを 1 プロセス。
- エンドポイント: task.md 4.2 の表に、`/ws`（`@app.websocket`）、`/delay/{ms}`（`async def` + `asyncio.sleep`、常駐のループ）、
  `/static`（`StaticFiles` 相当の `App` の静的ルート）を足す。軽いハンドラの `offload=False` は計測で決める。
- `completeness` の middleware は、`Depends`・CORS のネイティブ・ASGI ミドルウェアのどれを根拠にするかを standard の規則に照らして決める。

### 4. D（the benchmarker、`python/veil`）

- task.md 6 章のとおり（`GET /`・`GET /user/:id`・`POST /user`、ポート 3000、1 プロセス）。版を `veil-api==0.10.0` に。

### 5. 検証

- HttpArena: `./scripts/validate.sh veil` / `veil-api` / `veil-python` が購読するプロファイルで全部 PASS（ログを `results/` に）。
  ローカルの計測（`benchmark-lite.sh`）で errors 列と 0 rps を先に見る。nginx・axum・actix・fastapi・robyn 等の既存エントリと
  同じ条件で交互に回し、相対値を記録（ローカル機は計測機と桁が違うので参考値）。`ss -ltnup` でポートの持ち主が 1 プロセス。
- the benchmarker: `bundle exec rake config` → `make -f python/veil/.Makefile build test unbuild` が PASS。
- 1 プロセス化の効果: v0.8.4 の 2〜3 プロセス構成と v0.10.0 の 1 プロセス構成で、アイドル 60 秒の `cpu.stat` の差分と RSS を比べて記録
  （latency-10k・メモリボーナスへの影響の根拠）。

### 6. 提出（**公開行為なので、各手順の前に人の確認を取る**）

- 各リポジトリの fork・ブランチ・push・PR の作成・PR へのコメント（HttpArena の `/benchmark -f <name>` はメンテナが実行）は、
  内容（差分・PR 本文の下書き `docs/artifacts/benchmark/pr/<entry>.md`）を人に見せて承認を得てから行う。
- HttpArena は 1 エントリ 1 PR を基本にする（レビューしやすさ）。3 エントリをまとめるかは提出前に人が決める。
- 計測結果のコメントを見て、設定・エントリ側だけで直せるものは直す。本体の不具合なら本体のチケットを起票し、そのプロファイルは購読から外す。

## 受け入れ条件

- A・B・C・D の validate / test が PASS し、ログ・計測の記録が `docs/artifacts/benchmark/results/` にある。
- 人の承認の後に PR が作られている（作った PR の URL を本チケットに記録する）。
- 提出物が公開済みの v0.10.0 だけを使っていることを Dockerfile・lock・起動ログで確かめている。
