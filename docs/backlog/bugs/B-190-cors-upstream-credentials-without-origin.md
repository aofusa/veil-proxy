# B-190: CORS: 上流が ACAO 無しで `Access-Control-Allow-Credentials` を返すと、値が重なるか設定に無い credentials が許可される

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 のデータプレーンのレビュー（`docs/artifacts/v0100/review-dataplane.md` の L-3）。F-273。

## 現象

1. `allow_credentials = true` のルートで、上流が `Access-Control-Allow-Credentials: true` だけを返す（ACAO 無し）と、veil は
   上流の行を残したまま自分の ACAO と ACAC を足すので ACAC が 2 行になる。ブラウザは値を `"true, true"` と結合して
   `"true"` と一致しないとみなし、CORS が失敗する。
2. `allow_credentials = false` のルートで同じ応答を受けると、veil の ACAO と上流の ACAC が揃い、設定で許可していない
   credentials 付きの読み取りが許可したオリジンに通る（許可したオリジンに限られる）。

## 原因

`CorsActual::apply_h1`・`apply_with`（`src/route_cors.rs`）は、上流の CORS のヘッダを除くのを「上流が ACAO を返し、かつ
`override_upstream = true`」のときだけにしていた。

## 直したこと

- 上流が ACAO を返し `override_upstream = false` なら今までどおり上流のものを残して何も足さない。veil が足すときは、
  `override_upstream` に関係なく上流の `Access-Control-Allow-Origin`・`-Allow-Credentials`・`-Expose-Headers` を除いてから
  足す（HTTP/1.1・HTTP/2・HTTP/3）。要求ごとの費用は、ヘッダを見る 1 回のループの中の比較が 1 つ増えるだけ（除くのは CORS の
  ヘッダがあったときだけ）。
- 文書: `docs/guide/routing.md`（英日）、`examples/config.toml` の `override_upstream`。

## 試験

- 単体 `route_cors::tests::upstream_credentials_without_origin_are_replaced`（HTTP/1.1 の head と HTTP/2・HTTP/3 のヘッダの列、
  `override_upstream` の両方。直す前は上流の ACAC が残る）。
