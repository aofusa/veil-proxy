# F-175: 上流 HTTPS の HTTP/2（ALPN）

**状態: 完了（feat/v080-limitations）**

B-83 で上流 TLS の ALPN を `http/1.1` のみにした（h2 を提示しながら HTTP/1.1 を送って 502 になっていたため）。
本チケットで TLS 上の HTTP/2 上流を実装する。

## 設計

- 上流ごとの設定 `http2 = "auto" | "on" | "off"`。**既定は `"auto"`**（ALPN に `h2, http/1.1` を提示し、
  ネゴシエーション結果で HTTP/2（F-174）と HTTP/1.1 を切り替える）。後方互換は考慮しない（v0.8.0 の方針）。
- `ClientConfig` は ALPN 違いの系統を設定ロード時に作って `Arc` で持つ（要求ごとに作らない）。
- HTTP/1.1・HTTP/2・HTTP/3 で受けた HTTPS 上流すべてに適用する。

## 結果（2026-10-09）

- 設定 `http2 = "auto" | "on" | "off"`（`[upstreams.X]`・サーバーエントリ・ルートの `url`。既定 `"auto"`）。
  `ProxyTarget.http2` に解決し、`h2_over_tls()` で判定する。
- ALPN `h2, http/1.1` のクライアント設定（`configure_alpn_h2_client`）と、ワーカーごとのコネクター
  （`get_tls_connector_h2`）を追加。ALPN の結果は kTLS 移行前に記録する（`KtlsClientStream::negotiated_h2`）。
- `upstream_mux::open_https`: プールの HTTP/2 接続 → 無ければ ALPN `h2, http/1.1` で接続し、`h2` なら多重化接続
  （F-174）、`http/1.1` ならネゴシエーションに使った接続を呼び出し側の HTTP/1.1 経路へ渡す（接続を無駄にしない）。
  HTTP/1.1 を選んだ上流はワーカーごとに 5 分間覚え、その間は ALPN `http/1.1` だけで接続する。`"on"` で h2 以外なら 502。
- フロントエンド:
  - HTTP/2: バッファ経路（`h2_emit_upstream_response`。非 gRPC は圧縮も適用）・ストリーミング経路
    （`h2_relay_h2_streaming`、全二重）。gRPC も HTTPS 上流の h2 でストリーミング経路へ。
  - HTTP/3: ストリーミング経路（`open_h2_tls` → `relay_h2_stream`）・バッファ経路（`request_https_buffered`）。
  - HTTP/1.1: **`"on"` のときだけ** HTTP/2（`proxy_h1_via_https_h2`）。`"auto"` では HTTP/1.1 のまま（下記の計測）。

計測（交互 A/B、nginx（TLS・`http2 on`）を上流に、h2load `-c100`、base = F-171 のコミット＝上流は常に HTTP/1.1）:

| クライアント | サイズ | base | new | 差 |
|---|---|---|---|---|
| HTTP/2（`-m10`） | 3B | 13,627 | 24,465 | +80% |
| HTTP/2（`-m10`） | 54KB | 3,400（エラー 200 件） | 3,690（エラー 0） | +8.5% |
| HTTP/3（`-m10`） | 3B | 13,482 | 20,929 | +55% |
| HTTP/3（`-m10`） | 54KB | 1,480 | 1,473 | 差なし |
| HTTP/1.1（`-m1`、h2 を使った場合） | 3B | 11,267 | 9,886 | −12% |
| HTTP/1.1（`-m1`、h2 を使った場合） | 54KB | 3,285 | 2,842 | −13.5% |

HTTP/1.1 のクライアントはクライアント接続ごとにプールの上流接続を持てるので多重化の利点が無く、多重化接続の
アクターを経由するぶん遅くなる。このため `"auto"` は HTTP/2・HTTP/3 のクライアントにだけ HTTP/2 を使う。
- E2E: `test_f175_https_upstream_h2_from_http1_client`（`"on"` / `"off"` / `"auto"`）、
  `test_f175_https_upstream_h2_from_h2_and_h3_clients`、`test_f175_http2_on_rejects_http1_only_upstream`（502）。
  既存の HTTPS 上流の E2E はすべて `"auto"`（HTTP/1.1 しか話さない上流への切り替え）を通る。
