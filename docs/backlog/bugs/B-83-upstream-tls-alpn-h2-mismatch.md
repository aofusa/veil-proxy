# B-83: 上流 TLS の ALPN で `h2` を提示するのに HTTP/1.1 を喋る

## 事象

`https://` バックエンドへ接続する際、veil の上流 rustls クライアントは
`configure_alpn_h2_client(config, false)`（`src/config.rs` の `TLS_CONNECTOR` ほか計 4 箇所）で
ALPN に **`["h2", "http/1.1"]`** を提示する。しかし `proxy_https_pooled` /
`connect_https_backend_fresh` / `h2_proxy_https` / `http3_stream::run_backend_task` の
いずれも **ネゴシエート結果を一切参照せず、常に HTTP/1.1 のリクエストを書き込む**
（`src/proxy.rs` に `alpn` の文字列は 1 つも存在しない）。

したがって **上流が h2 を選択できる HTTPS バックエンドでは中継が壊れる**:

- バックエンド（veil）側ログ: `Invalid preface received: [71, 69, 84, ...]`（= `GET / HTTP/1.1`）
  → `[HTTP/2] Handshake error: Invalid connection preface`
- veil（プロキシ）側: `Backend closed connection without sending response` → **502**

## 再現

`tests/e2e_setup.sh` のバックエンド設定に `http2_enabled = true` を足して
（= TLS リスナーが ALPN `h2, http/1.1` を広告するようにして）、そのバックエンドを
`https://` アップストリームとして参照するルートへリクエストする。

F-170 の E2E で UDS バックエンドに `http2_enabled = true` を書いたところ再現した。
**既存の E2E バックエンド（backend1 / backend2）は `http2_enabled` を設定していない**
＝ TLS リスナーが ALPN を一切広告しないため、これまで一度も表面化していなかった。

## 影響

nginx・Envoy・多くのアプリサーバは HTTPS リスナーで既定 h2 を広告する。
**実運用で `https://` 上流を使うと、その上流が h2 対応なら常に 502 になる**可能性がある。
veil 同士の多段構成でも、後段が `http2_enabled = true` なら壊れる。

## 改修案

いずれか。

1. **上流クライアントの ALPN から `h2` を外す**（`configure_alpn_h2_client` を
   HTTP/1.1 のみにする、または上流用の別関数を用意する）。最小変更で確実。
   HTTP/2 上流が必要な構成は既に `use_h2c`（h2c プール、F-106）が担っているが、
   **TLS 上の h2 上流（`https://` + HTTP/2）は現状サポートしていない**ため、
   ALPN で h2 を提示しているのは実装と矛盾している。
2. ハンドシェイク後に `alpn_protocol()` を見て `h2` なら HTTP/2 クライアントで
   喋る（= TLS 上の h2 上流を本実装する）。こちらが本筋だが範囲が大きい。

まず 1 で実害を止め、2 は別チケット（機能追加）にするのが妥当。

## 発見

F-170（UDS バックエンド）の E2E 追加時。UDS とは無関係で、TCP の `https://` 上流でも同じ。
