# F-171: h2c 上流の全二重ストリーミング対応

**状態: 完了（feat/v080-limitations）**

B-84（HTTP/3 のストリーミングバックエンド経路が `use_h2c` を無視する）の修正で
「HTTP/3 → h2c 上流」はサーバ選択直後にバッファ経路（`Decision::Buffer` ->
`handle_request` -> `proxy_to_h2c_backend_async`）へ回すようになり、502 は解消した。
しかし**全二重 h2c 上流ストリーミングそのものは未実装のまま**である。本チケットは
その残件を機能追加として起票する。

## 現状（B-84 修正後）

- `H2cClient::send_request`（`src/http2/client.rs`）は完全バッファ型:
  リクエストボディを `Option<&[u8]>` で受け取り、レスポンスも `H2cResponse`
  （ステータス・ヘッダ・ボディ全体）としてまとめて返す。
- HTTP/2 クライアント経路（`proxy.rs::h2_proxy_h2c`）も同じくバッファ型。
- HTTP/3 の h2c 上流は、単発 RPC・サーバストリーミング RPC（クライアント→サーバは
  1 メッセージ、サーバ→クライアントのみストリーム）であれば、リクエストを
  バッファしてから送っても意味的には成立する（レスポンスは
  `http3_server.rs::handle_request` 側で応答をストリーム転送できる）。
- **クライアントストリーミング RPC・双方向ストリーミング RPC**
  （クライアントが複数メッセージを送り続け、サーバがその途中で応答を返し得るもの）は
  バッファ経路では表現できない。リクエストボディ全体を受信し切るまで上流への送信を
  開始できないため、真の全二重にならない。

## 影響

**gRPC over HTTP/3 のストリーミング RPC 中継が未対応。** `tools/perf` の gRPC
ストリーミングシナリオを HTTP/3 クライアントで計測・運用する場合に制約となる。

## 改修案

`BackendTaskParams`（`src/http3_stream.rs`）に `use_h2c: bool` を追加し、
`run_backend_task` から h2c 上流へ直結する経路を作る。

1. **`H2cClient` にストリーミング API を追加する。**
   `send_request` とは別に、リクエストヘッダ送信後にボディチャンクを逐次
   `send_data` できる形（h2 クレートの `SendStream` 相当をラップする）と、
   レスポンスヘッダ受信後にボディチャンクを逐次受け取れる形（`RecvStream` 相当）
   が要る。既存の完全バッファ API は変更しない（他の呼び出し元との後方互換）。
2. **ストリーム対応付けの検討。** `http3_stream.rs` の `RespMsg` チャネル設計
   （HTTP/3 ストリームごとに `req_rx` / `resp_tx` を持つ非同期タスク）が、
   h2 のストリーム ID ベースの多重化とどう噛み合うかを事前に設計すること。
   `H2C_POOL`（コネクションプール、F-106）の 1 コネクションを複数の HTTP/3
   ストリームが同時に使う場合、h2 側のストリームごとの送信ウィンドウ管理
   （F-116 で踏んだ「消費連動ウィンドウ補充」の教訓）に注意する。
3. **ホットパス絶対規則を守ること。** ストリーミング化のためにチャンクごとの
   新規アロケーションを増やさない。`Bytes` の参照カウント共有で完結させる。

## 受け入れ条件

- gRPC のクライアントストリーミング・双方向ストリーミング RPC を HTTP/3 経由で
  h2c 上流へ中継でき、レスポンスの初送出がリクエスト完了を待たない
  （= 全二重であることをテストで確認できる）。
- 既存のバッファ経路（単発・サーバストリーミング RPC、HTTP/1.1・HTTP/2 クライアント）
  の挙動・性能を退行させない。
- ホットパス絶対規則（同期 I/O 禁止・不要アロケーション禁止・ゼロコピー）を守る。

## 依存・リスク

- B-84（完了）の上に乗る。
- `H2cClient` のストリーミング API 新設は影響範囲が広く、h2 クレートのストリーム
  ライフサイクル・エラーハンドリング（ストリームリセット・ウィンドウ枯渇）を
  丁寧に扱う必要がある。F-116（HTTP/2 多重化アクターモデル）で踏んだ
  「ReadFuture drop でデータ破棄」「送信ウィンドウ枯渇」系の落とし穴を
  再発させないよう、実装前に F-116 の記録を参照すること。

## 改修方針（2026-10-09、feat/v080-limitations）

F-174（多重化対応の上流 HTTP/2 クライアント）の上に実装する。`BackendTaskParams` に上流プロトコル
（HTTP/1.1 / h2c / TLS 上の h2）を持たせ、`classify` の B-84 分岐（h2c → バッファ経路）をストリーミング経路へ変える。
gRPC も B-97 の改修で `RespMsg::Trailers` を持てるようになるため、ストリーミング経路で扱う。

## 結果（2026-10-09）

- HTTP/3: `BackendTaskParams.h2`（`H2Upstream`: method / `:path` / authority / 所有ヘッダ）を追加し、`classify` で
  h2c 上流（gRPC を含む）をストリーミング経路へ回す（B-84 のバッファ経路への迂回を廃止）。`run_h2_task` は
  F-174 の多重化接続でストリームを開き、要求本文の中継と応答（head / DATA / `RespMsg::Trailers`）の中継を
  同じタスクで並行に進める。圧縮対象（非 gRPC）の応答だけは本文を集めてから圧縮する。
- HTTP/2: `h2_route_streaming_plan` が h2c 上流（gRPC を含む、gRPC は `buffering = full` をバイパス）を
  ストリーミング適格にし、`h2_relay_h2c_streaming` が同じ形で全二重に中継する。上流の選択は Consistent Hash の
  header/cookie キーに対応した `select_with_header_fn`（バッファ経路と同じ）。
- trailers-only の gRPC 応答（HEADERS + END_STREAM）は、下流でも fin / END_STREAM 付き HEADERS 1 枚で返す。
- gRPC を HTTP/1.1 上流へ送る構成・WASM 適用ルート・`buffering = full`（gRPC 以外）はバッファ経路のまま。
- E2E: `test_f171_http3_grpc_bidi_streaming_is_full_duplex` / `test_f171_http2_grpc_bidi_streaming_is_full_duplex`
  （双方向ストリーミングで、要求ストリームを閉じる前に 1 通目の応答が届く。旧実装はタイムアウトすることを確認済み）。

計測（交互 A/B、gRPC unary を h2load `-c100 -m10`、base = F-174 のコミット。unary はバッファ経路 → ストリーミング経路）:

| フロント | base | new | 差 |
|---|---|---|---|
| HTTP/2 | 18,855 | 18,891 | +0.2%（ノイズ） |
| HTTP/3 | 22,823 | 24,482 | +7.3%（4 ラウンド全勝・分布分離） |
