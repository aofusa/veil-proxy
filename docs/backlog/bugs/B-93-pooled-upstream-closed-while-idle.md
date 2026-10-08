# B-93: 上流が閉じたプール済み keep-alive 接続を再利用して 502 を返す

## 事象

E2E 全体実行で `test_b17_bad_backend_no_response_returns_504` が稀に 502 で落ちる
（単独実行では毎回通る）。

## 原因

`HttpConnectionPool::get` / `HttpsConnectionPool::get` はアイドルタイムアウトしか見ておらず、
**上流がプール中の接続を閉じていても再利用していた**。要求を書いた先で EOF を読み、
B-17 の「ヘッダー完了前の EOF → 502」経路でクライアントへ 502 を返す。

E2E の bad-backend は通常応答を `Connection: close` 無しで返した直後に接続を閉じるため、
veil はそれを keep-alive としてプールし、同じワーカーの次の要求（`no-response` 等）が
閉じた接続を掴むと 502 になる。実運用でも、上流のアイドルタイムアウト
（Node.js `keepAliveTimeout` 既定 5 秒、nginx `keepalive_timeout` 等）が veil の
`idle_connection_timeout_secs`（既定 30 秒）より短いと、境界付近の要求が 502 になる。
nginx は同じ状況で（キャッシュ済み接続なので）別接続へ再試行する。

さらに平文 HTTP で上流が `Content-Length` を超えて送ってきた場合、残骸が受信バッファに
残った接続を再利用すると次の応答とずれる（desync）。

## 修正

プール取得時に、**アイドルが 1ms 以上の接続だけ**非ブロッキングの
`recv(MSG_PEEK | MSG_DONTWAIT)`（Windows は非ブロッキングソケットへの `MSG_PEEK`）で
生存確認し、EOF・エラーなら破棄する。平文 HTTP では未読データがあっても破棄する
（TLS は NewSessionTicket 等の post-handshake メッセージが正当に残り得るため破棄しない）。

- 閾値未満の接続は確認しない: 高負荷時はプールへ返した接続が数十 µs で再利用されるため、
  毎回確認すると「1 プロキシ要求 1 syscall」の固定費になる（ホットパス絶対規則）。
  閾値未満の窓（FIN がまだ届いていない）は MSG_PEEK でも原理的に検出できない。
- 再試行方式（nginx 相当）は採らなかった: 要求 `Vec` の所有権が各転送経路
  （kTLS splice・バッファリング・圧縮）へ移るため、成功経路でも要求の複製（確保）が要る。

## テスト

- 単体: `pool::pooled_liveness_tests`（Idle / HasData / Closed の判定、閾値未満は確認しない、
  平文と TLS で未読データの扱いが異なる、アイドルタイムアウト）
- E2E: `test_b93_pooled_upstream_closed_while_idle_is_not_reused`（同じ keep-alive 接続で
  ok-baseline を 100ms 間隔で 3 回。修正前は 2 回目が決定的に 502）
