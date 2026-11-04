# B-267: `add_response_headers`・`remove_response_headers` が HTTP/2・HTTP/3 のプロキシの応答と一部の HTTP/1.1 の経路で効かない

**状態: 修正済み（v0.10.1）**

## 出典

[B-266](B-266-head-local-responses-send-body.md) と同じ調査（プロトコル間の機能の差の洗い出し）。v0.10.0 のソースのデバッグビルドに
`add_response_headers = { "X-Added" = "1" }`・`remove_response_headers = ["Server"]`（h2c の上流は `["X-Up"]`）のルートを置き、
平文 HTTP/1.1 の上流（Python の `http.server`、`Server` を返す）と h2c の上流（veil、`X-Up` を返す）で確かめた。

## 事象

ガイド（`routing.md` の Response Header Manipulation）は「クライアントへ送る前にヘッダを足す・消す。静的配信にも効く」と書き、
例は HSTS・`X-Frame-Options`・`X-Content-Type-Options` を足して `X-Powered-By` を消す。実際に効くのは次の表の ✓ だけ。

| クライアント → 上流 | 足す | 消す | 確かめ方 |
|---|---|---|---|
| HTTP/1.1 → HTTP/1.1（平文・TLS、ストリーミング・圧縮） | ✓ | ✓ | 実機 |
| HTTP/1.1 → HTTP/1.1、`buffering = "full"`（`proxy::buffered`） | **×** | **×** | 実機 |
| HTTP/1.1 → h2c（`proxy_h2c`） | **×** | **×** | 実機 |
| HTTP/1.1 → TLS の h2（`proxy_h1_via_https_h2`、`http2 = "on"`） | **×** | **×** | コード |
| HTTP/2 → HTTP/1.1（`h2_relay_backend_response`、平文・TLS） | **×** | **×** | 平文の上流は実機（クライアントは h2c・TLS の HTTP/2 とも）、TLS の上流はコード |
| HTTP/2 → h2c・TLS の h2（`h2_emit_upstream_response`・`h2_relay_h2_streaming`） | **×** | **×** | h2c は実機、TLS の h2 はコード |
| HTTP/3 → すべて（`http3_stream` のストリーミング・`request_task::handle_proxy`） | **×** | **×** | コード（H3 の経路に `remove_response_headers` の参照が無い） |
| 静的配信（File・memory。全プロトコル） | ✓ | 対象外 | 実機・コード |
| `type = "Respond"`・`type = "Redirect"`・エラー応答（全プロトコル） | **×** | 対象外 | 実機・コード |

- `buffering = "full"` の HTTP/1.1 の経路は、上流のステータス行をそのまま返す（上流が `HTTP/1.0 200 OK` なら veil も `HTTP/1.0` で返す）。
  他の HTTP/1.1 の経路は `compress::render_response_head` で `HTTP/1.1` に直している。
- CORS（F-273）の応答ヘッダはどの経路でも足している（`route_cors::apply_*`）。ルートの方針のうち応答ヘッダだけが経路ごとにばらばら。

## 影響

- P1（HTTP/2・HTTP/3 で受けるプロキシのルートすべて。ブラウザは普通 HTTP/2 か HTTP/3 で来る）:
  - 足すはずのセキュリティヘッダ（HSTS・CSP・`X-Frame-Options` 等）が付かない。
  - 消すはずの上流のヘッダ（`Server`・`X-Powered-By`・内部のデバッグ用ヘッダ）がクライアントへ漏れる。
  - 既存の E2E は HTTP/1.1 でしか応答ヘッダの方針を見ていなかった。

## 修正方針

- 応答ヘッダの方針を 1 か所の関数にまとめ、全経路の応答の head を作るところで呼ぶ:
  - HTTP/2・HTTP/3（`Vec<(Bytes, Bytes)>`・`RespHeaders`）向けに `remove_response_headers` を落として `add_response_headers` を足す関数を作り、
    CORS を足す箇所（`route_cors::apply_bytes`）と同じ場所で呼ぶ。HTTP/3 のストリーミングは `BackendTaskParams` に方針を渡す
    （`cors` と同じく `Arc` の clone だけ）。
  - HTTP/1.1 の `proxy_h2c`・`proxy_h1_via_https_h2`・`proxy::buffered` は `render_response_head` と同じ処理にする（ステータス行も `HTTP/1.1` に直る）。
- 方針の無いルート（`has_header_operations()` が偽）は分岐 1 回だけで、要求ごとの確保・コピーを増やさない（ホットパス規則）。
  足すヘッダの名前・値はロード時に `Bytes` にしておく。
- `Respond`・`Redirect`・エラー応答に付けるかを決めて、ガイドに書く（nginx の `add_header` は既定で 2xx・3xx だけ、`always` で全部）。
  付けるなら `FixedResponse` はロード時に組み込めるので要求ごとの費用は無い。
- 試験: E2E で HTTP/1.1（平文・TLS・`buffering = "full"`）・h2c・HTTP/2・HTTP/3 × 上流 HTTP/1.1・HTTPS・h2c・TLS の h2 の応答で、
  足すヘッダがあり消すヘッダが無いこと。交互 A/B で方針の無いルートの性能が変わらないこと（3B・54KB、io_uring と epoll）。

## 修正（v0.10.1）

- `[route.security] add_response_headers`・`remove_response_headers` を全プロトコル・全経路（プロキシのストリーミング・圧縮・
  `buffering = "full"`・h2c と TLS の h2 の上流・静的配信・`Respond`）にかける。HTTP/2・HTTP/3 は `SecurityConfig::apply_response_headers`
  （追加するヘッダは設定ロード後の初回に `Bytes` にしておき、要求ごとは参照カウントだけ）、HTTP/1.1 は `render_response_head`。
  `Respond` の応答は設定ロード時に 1 回だけかける（`FixedResponse::with_security`）。`buffering = "full"` のステータス行は `HTTP/1.1` に直す。
- `remove_response_headers` に `Server`・`Alt-Svc` を書くと veil 自身の `Server`・HTTP/3 の広告も付けない（全プロトコル）。
- kTLS の splice 経路の同じ漏れは [B-275](B-275-ktls-splice-raw-response-head.md)。
- 統合試験 `b267_b271_header_policy_alt_svc_and_server_header_match_across_protocols`。

## 関連

- [B-266](B-266-head-local-responses-send-body.md)、[B-268](B-268-wasm-response-headers-skipped-on-some-paths.md)（同じ経路の Wasm の応答ヘッダ）、
  [B-271](B-271-alt-svc-missing-on-some-h1-paths.md)（同じ経路の Alt-Svc）、[B-133](B-133-respond-redirect-ignore-route-security.md)（Respond・Redirect と
  ルートのセキュリティ設定）、[F-273](../features/F-273-cors-actual-responses.md)
