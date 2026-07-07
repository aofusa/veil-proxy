# B-28: HTTP/2 逆プロキシがバックエンド接続を再利用せずエフェメラルポート枯渇（502）

- **優先度**: P1
- **状態**: 完了（2026-07-07 修正・検証済み）
- **検出**: `tools/perf`（0.5.0 リリース前ベンチ、2026-07-07）の `h2_1_feat_proxy` / `h2_1_feat_buffering` HTTP/2 計測

## 事象

HTTP/2 フロントエンドの逆プロキシ構成に `h2load -n 30000 -c100 -m10` を実行すると、
約 28,000 リクエスト以降で **5xx が 1,000〜1,500 件** 発生し、スループットも
475 req/s 程度に低下する（`-n 10000` では発生しない）。veil のログ:

```
WARN [src/proxy.rs] [HTTP/2] Backend connect error: Cannot assign requested address (os error 99)
```

## 調査（2026-07-07・原因確定）

- HTTP/1.1 フロントエンド経路（`proxy_http_pooled`）はスレッドローカル `HTTP_POOL` で
  バックエンド接続を keep-alive 再利用するが、**HTTP/2 フロントエンド経路
  （`handle_http2_proxy_http` / `handle_http2_proxy_https`）はリクエスト毎に新規接続**し、
  応答転送後にドロップ（veil 側 active close）していた。
- リクエストには `Connection: keep-alive` を送っているためバックエンドは接続を維持し、
  veil 側クローズで **TIME_WAIT が veil 側に蓄積**。エフェメラルポート範囲
  （既定 32768–60999 ≒ 28,232 個）を使い切ると `connect(2)` が
  `EADDRNOTAVAIL`（os error 99）で失敗し 502 になる。
- `-n 30000 > 28,232` で閾値を跨ぐため、ベンチ後半でのみ顕在化していた。
  実運用でも高スループットの HTTP/2 フロントエンドで同じ枯渇が起こり得る。
  接続毎のハンドシェイクコストにより正常時のスループットも大きく損なわれていた。

## 修正内容（2026-07-07）

`src/proxy.rs`:

- `relay_h2_response` の戻り値に**バックエンド接続の再利用可否**を追加。
  Content-Length を全量ちょうど消費し `Connection: close` でない場合のみ `true`
  （CL ストリーミング経路・バッファリング経路とも）。chunked・EOF 終端・エラー・
  打ち切りは残データ混入の恐れがあるため `false`（従来どおりクローズ）。
- `handle_http2_proxy_http`: HTTP/1.1 経路と同じ `HTTP_POOL`（キー `host:port` 共有）
  から取得・返却するプーリングを実装。
- `handle_http2_proxy_https`: `HTTPS_POOL`（キー `addr:sni`）で TLS 済み接続を
  プーリング（TLS ハンドシェイク削減 + ポート枯渇防止）。
- `Backend::Proxy` のルート別 `SecurityConfig`（`max_idle_connections_per_host` /
  `idle_connection_timeout_secs`）をプール返却パラメータとして配線。

残件（別チケット候補）: chunked 応答の境界（トレーラー含む）を正確に消費して
再利用可能にする拡張、H2C バックエンド（`handle_http2_proxy_h2c`）の接続再利用。

## 検証

- `h2load -n 30000 -c100 -m10`（feat_proxy / feat_buffering、glibc/musl）:
  EADDRNOTAVAIL・5xx が解消し 30000/30000 成功すること（結果は `docs/perf/` 参照）。
- 既存 E2E（プロキシ・gRPC・H2C・ストリーミング系）全通過。

## 関連

- [B-25](B-25-reverse-proxy-http1-wrk-zero-completed.md) / [B-27](B-27-ktls-http2-short-write-frame-desync.md):
  同じ perf 拡充シリーズで検出された kTLS 送信経路のバグ。B-27 修正で HTTP/2 の
  スループットが回復した結果、本件（30000 リクエスト到達によるポート枯渇）が顕在化した。
