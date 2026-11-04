# B-275: kTLS の splice 経路が上流の応答ヘッダをそのまま送り、応答ヘッダの方針・`Server`・`Alt-Svc` が効かない

**状態: 修正済み（v0.10.1）**

## 出典

[B-267](B-267-response-header-policy-missing-on-proxy-paths.md) の修正中に、HTTP/1.1 のプロキシの応答の送り出しの経路を全部たどって見つけた（コードで確認）。

## 事象

kTLS を有効にした HTTPS のリスナーで、HTTP/1.1 のプロキシの応答を `splice(2)` で送る経路（`splice_transfer_response_ktls`）は、
上流の応答の head を `async_raw_write_all` でそのままクライアントへ書いていた。他の経路（`render_response_head`）と違い、

- `[route.security] add_response_headers`・`remove_response_headers` がかからない（HSTS が付かず、上流の `Server` 等が漏れる）
- veil の `Server`（`server_header_value`）に置き換わらず、`Alt-Svc`（HTTP/3 の広告）も付かない
- 上流が `HTTP/1.0` で返すとステータス行の版もそのまま返す

kTLS のときだけ起きるので、kTLS を使わない E2E・単体試験では見えなかった。

## 修正（v0.10.1）

- 応答ヘッダの方針・`Alt-Svc`・`Server` の置き換えのどれかが要るか、ステータス行が `HTTP/1.1 ` でなければ、他の経路と同じ
  `render_response_head` で head を作り直してから送る（`src/proxy/splice.rs`）。どれも要らないときは今までどおり上流の head を
  確保せずに送る（ホットパスのコストは分岐だけ）。

## 関連

- [B-267](B-267-response-header-policy-missing-on-proxy-paths.md)、[B-271](B-271-alt-svc-missing-on-some-h1-paths.md)
