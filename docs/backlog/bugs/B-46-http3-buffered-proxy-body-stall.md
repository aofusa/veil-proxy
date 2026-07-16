# B-46: HTTP/3 バッファ経路のプロキシ応答でボディが送出されずストリームエラーになる

## 事象

F-118 の全構成 perf 計測で、`h3_proxy_buffering`（HTTP/3 逆プロキシ + `[route.buffering]
mode = "full"`）の **http3 プロトコルのみ 0 req/s**（glibc / musl 両方で再現）。
h2load (QUIC) のログでは:

```
requests: 30000 total, 24316 started, 23388 done, 0 succeeded, 30000 failed, 30000 errored
status codes: 23388 2xx, 0 3xx, 0 4xx, 0 5xx
traffic: 29.79MB total, 2.23MB headers, 0B (0) data
```

**2xx ヘッダは届くがレスポンスボディ（DATA）が 0 バイト**で、全ストリームが
エラー終了する。同一構成の http1.1 / http2 は正常（ボディあり）。
バッファリングなしの `h3_proxy`（streaming 経路）も正常（~660 req/s、エラー 0）。

## 調査（初期切り分け）

- HTTP/3 は `mode = "full"` のとき streaming Decision をバイパスし
  **バッファ経路**（`http3_server.rs::handle_request` → `handle_proxy` →
  `send_response(stream_id, status, headers, Some(body))`）を通る
  （`http3_stream.rs` の `Decision::Buffer` 分岐）。
- 症状（ヘッダのみ届きボディが永久に届かない・ストリームエラー）は **B-43**
  （StreamBlocked 後のヘッダ/ボディ順序不整合によるストリームスタック）と同型。
  B-43 の修正は静的 File 応答の `PartialResponse` 経路で検証済みだが、
  53KB 応答（新規接続の輻輳ウィンドウ ~12KB 超）をバッファ経路の `send_response`
  で送る本ケースで再発している可能性が高い。
- 既存 E2E（`test_http3_buffering_spillover` 等）は「応答があること」しか検証して
  おらず、**ボディ内容の一致を検証していない**ため検出できなかった。

## 再現手順（想定）

```bash
# h3_proxy_buffering 構成で veil + perf-backend を起動し
h2load --alpn-list=h3 -n 10 -c 1 -m10 https://<veil>:443/
# → 2xx ヘッダのみでボディ 0B、failed/errored
```

## 改修案

1. B-43 の再現手法（`-n10 -c1 -m10`）でバッファ経路の送出をトレースし、
   `send_response`/`try_flush_partial` のヘッダ→ボディ順序・PartialResponse 保持を
   バッファ経路（proxy 応答）でも B-43 と同じ一元化に載せる。
2. 回帰テスト: HTTP/3 + buffering full のプロキシ応答で**ボディ内容が上流と一致**
   することを検証する E2E を追加（既存の liveness のみの検証を強化）。

## 影響・優先度

- 影響構成: HTTP/3 + `[route.buffering] mode="full"` の Proxy ルートのみ。
  HTTP/1.1・HTTP/2・streaming（バッファリングなし）の HTTP/3 Proxy・File 配信は正常。
- 検出: F-118 perf 計測（2026-07-15 run。`docs/artifacts/perf_reports/f118/`）

## 関連

- B-43（HTTP/3 StreamBlocked → FrameUnexpected スタック）/ F-32（HTTP/3 ストリーミング）
- F-42（buffering の非同期 FS 化）
