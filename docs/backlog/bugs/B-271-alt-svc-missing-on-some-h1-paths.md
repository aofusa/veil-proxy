# B-271: HTTP/3 の広告（`Alt-Svc`）が一部の応答に付かない・HTTP/3 だけ `server` ヘッダを返す

**状態: 修正済み（v0.10.1）**

## 出典

[B-266](B-266-head-local-responses-send-body.md) と同じ調査。v0.10.0 のソースのデバッグビルドで `[http3] enabled = true`（`alt_svc_enabled` は既定の true）にし、
経路ごとに `Alt-Svc` の有無を見た。

## 事象

`[http3]` の `alt_svc_enabled` は「HTTP/1.1 と HTTP/2 の応答で HTTP/3 を広告する」設定（F-94）。付かない応答がある。

| 応答 | HTTP/1.1 | HTTP/2 |
|---|---|---|
| プロキシ → HTTP/1.1 の上流（ストリーミング・圧縮） | ✓ | ✓ |
| プロキシ、`buffering = "full"`（`proxy::buffered`） | **×**（実機） | ✓ |
| プロキシ → h2c の上流（`forward::proxy_h2c`） | **×**（実機） | ✓ |
| プロキシ → TLS の h2 の上流（`proxy_h1_via_https_h2`） | **×**（コード） | ✓ |
| 静的配信（File・memory） | ✓ | ✓ |
| `type = "Respond"` | **×**（実機） | **×**（実機。`h2_base_headers(false)`） |

- ブラウザが HTTP/3 へ移るのは `Alt-Svc` を見たときなので、上の経路だけを使うサイト（h2c の上流の前段など）は HTTP/3 に移らない。
- HTTP/3 の静的配信・`Respond` は `server: veil/http3` を返すが、HTTP/1.1・HTTP/2 は `Server` を返さない（プロキシは上流の `Server` をそのまま返す）。
  製品名を出すかどうかがプロトコルで違う。

## 影響

- P3: HTTP/3 への移行がその経路で起きない。`server` の差は情報の出し方の不一致（攻撃面の差は小さい）。

## 修正方針

- `Alt-Svc` を付けるのを、応答の head を作る共通の処理（[B-267](B-267-response-header-policy-missing-on-proxy-paths.md) でまとめる関数）に寄せる。
  `buffering = "full"`・`proxy_h2c`・`proxy_h1_via_https_h2`・`Respond` で付ける。`Respond` は `FixedResponse` を組み立てるロード時に入れる
  （リロードで `alt_svc` が変わったら作り直す）。
- `server: veil/http3` は消すか、3 つのプロトコルでそろえる（既定は返さない。nginx の `server_tokens off` 相当）。
- 試験: E2E で各経路の HTTP/1.1・HTTP/2 の応答に `Alt-Svc` があること、`alt_svc_enabled = false` で無いこと、HTTP/3 の応答に `server` が無いこと。

## 修正（v0.10.1）

- `Alt-Svc` を HTTP/1.1 の全経路（`buffering = "full"`・h2c と TLS の h2 の上流・`Respond`）と HTTP/2 の `Respond` に付ける
  （`pool::append_own_alt_svc_line` 等、1 か所の判定）。HTTP/3 の `Server` も `server_header_value`（既定 `veil`）にそろえる。
- 統合試験 `b267_b271_header_policy_alt_svc_and_server_header_match_across_protocols`。

## 関連

- [F-94](../features/F-94-http3-grpc-report-items-1-11.md)、[B-267](B-267-response-header-policy-missing-on-proxy-paths.md)、[B-270](B-270-h2c-upstream-responses-not-compressed.md)
