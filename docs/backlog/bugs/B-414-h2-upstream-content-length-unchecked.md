# B-414: HTTP/2 の上流の応答の content-length を DATA の長さと照らさずに HTTP/1.1 のクライアントへ中継していた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-17）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「HTTP/2 upstream response content-length is never checked against DATA length」から確認。

## 事象

- `proxy::forward::proxy_h1_via_https_h2`（HTTP/1.1 で受けた要求を、ALPN で h2 を選んだ上流へ中継する経路。
  `http2 = "on"` のときだけ）は、上流の最初の `content-length` だけを読み、長さが分かれば framing を空にして
  上流の `content-length` の行を（食い違う行も含めて）全部そのままクライアントの head へ写していた。
- 本文は DATA をそのまま書き、書いた量を宣言した長さと照らさずに接続をキープアライブへ戻していた。上流が宣言より
  長い DATA を送ると、余りがクライアントの接続の次の応答として読まれる（RFC 9113 §8.1.1 が中継を禁じる不正な応答）。
  上流を握った攻撃者が、前段の接続を共有するプロキシの別の利用者の応答を差し替え得る。

## 対応

- 上流の `content-length` は、すべての行が同じ 10 進の数のときだけ使う。読めない値・食い違う値は 502。
- 長さが分かる本文は、自分の `Content-Length` の行を 1 行だけ付けて上流の行は写さない（本文の無い応答は従来どおり）。
- 宣言した長さを超える DATA は書かずに接続を閉じる。足りないまま終わった本文も接続を閉じて切り詰めを示す。

## テスト

- `proxy::forward::revalidation_request_tests::h2_upstream_content_length_rejects_conflicts`。
