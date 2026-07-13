# B-43: HTTP/3 静的応答が StreamBlocked 後にヘッダ未送出のまま送信不能（FrameUnexpected）

## 事象

HTTP/3 の File（静的配信）経路で、新規接続直後に並行ストリーム（h2load `-m10` 等）で
リクエストすると、**最初の 1 本しか成功せず残りが全て失敗**する。
サーバログには `[HTTP/3] send_body error: FrameUnexpected` が多発し、クライアント
（h2load/ngtcp2）は失敗ストリームの完了を **サーバの max_idle_timeout（30 秒）** まで待つ。

再現（perf ハーネスの h2_1_feat_http3 構成コンテナに対して）:

```
h2load --alpn-list=h3 -n 10 -c 1 -m10 https://veil:443/
# → requests: 10 total, 1 succeeded, 9 failed / finished in 30.05s
```

## 影響

- HTTP/3 スループット計測（F-111/F-114/F-115）の「HTTP/2 比 1/6」の**主因**。
  1 データグラム 1 syscall（F-111 の結論）は副次要因で、実体は「接続初期の並行
  ストリームがほぼ全滅 + 30 秒テール待ち」だった。リクエスト自体の処理限界は
  マージナル ~430 req/s/conn（2.3ms/req）で CPU には大きな余地がある。
- 失敗は h2load の `failed`（ストリームエラー）であり **Non-2xx には計上されない**ため、
  perf ハーネスの Errors 列（Non-2xx）では 0 と表示され長期間検出されなかった。

## 原因

`src/http3_server.rs` の `Http3Handler::send_response()`:

1. `h3_conn.send_response()`（HEADERS 送出）が `StreamBlocked` を返した場合
   （新規接続の輻輳ウィンドウ ~12KB に対し 53KB 応答が先行ストリームを占有すると発生）、
   **ヘッダ未送出のままボディだけ** を `partial_responses: HashMap<u64,(Vec<u8>,usize)>` へ
   保存して戻る。
2. 再送経路（`flush_partial_responses` / `handle_writable_streams`）は保存エントリに対し
   **いきなり `send_body()`** を呼ぶため、quiche h3 の状態機械が `FrameUnexpected` を返す。
3. エラーでエントリは破棄され、当該ストリームは HEADERS もボディも永遠に送られない
   （RESET も送られない）→ クライアントはアイドルタイムアウトまで待つ。

また、ボディ無し応答（リダイレクト・エラー応答）で HEADERS が `StreamBlocked` になると
何も保存されず、応答が **無言で消失** する（同根）。

## 修正方針

`partial_responses` を `(body, written)` タプルから
`PartialResponse { head: Option<Vec<h3::Header>>, body: Vec<u8>, written: usize }` へ拡張:

- `send_response()` の `StreamBlocked` 時は **構築済みヘッダを保存**（ボディ無しなら空 body）。
- 再送は共通ヘルパーで「head があればまず `send_response()`（fin = body 空）→ 成功したら
  head を消してボディ送出へ」を行い、`StreamBlocked` の間はエントリを保持する。
- 成功経路（ヘッダ+ボディが 1 回で送れるケース）の追加アロケーションは無し
  （StreamBlocked のバックプレッシャ経路のみ保存が発生する。既存挙動と同等）。

## 検証

- h2load `-m10`（新規接続 × 並行ストリーム）で failed=0 になること。
- perf `h2_1_feat_http3` の req/s 改善（30 秒テール消滅）と HTTP/3 E2E 全通過。

## 関連

- [F-115](../features/F-115-http3-recvmmsg-sendmmsg-batching.md)（第2段の A/B 計測中に発見）
- [B-42](B-42-http3-proxy-load-instability.md)（h3_proxy の不安定性。プロキシ経路の head 保留は
  `HeadSend::Blocked` で別途実装済みだが、静的経路は本バグが残っていた）
