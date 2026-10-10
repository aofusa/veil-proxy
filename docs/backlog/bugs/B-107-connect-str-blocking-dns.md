# B-107: 上流のホスト名をイベントループ上の同期 getaddrinfo で解決している

**状態: 完了（feat/v080-limitations）**

## 事象

`runtime::TcpStream::connect_str("host:port")`（io_uring / epoll・kqueue / Windows の 3 実装）は、
`host` が IP リテラルでないとき `ToSocketAddrs`（同期 `getaddrinfo`）をワーカースレッド上で呼んでいた。
上流 URL にホスト名を書いた構成（`url = "http://api.internal:8080"` 等）では、接続を張るたびに
DNS 往復の間イベントループ全体が止まる。DNS サーバが遅い・落ちていると、そのワーカーの全接続が
`getaddrinfo` のタイムアウト（既定 5 秒 × 試行回数）だけ停止する。

設定ロード時に IP リテラルだけを `SocketAddr` へ解決しておく最適化（`ProxyTarget.socket_addr`）は
あったが、ホスト名の上流と、`connect_str` を直接使う経路（HTTP/3 の上流・h2c 上流・L4 以外の新規接続）は
対象外だった。v0.8.0 の設計調査（`docs/artifacts/v0.8_limitations_design.md`）で発見。

## 改修

- `runtime::dns::resolve`（新設）: IP リテラルはパースのみ。ホスト名はワーカーごとのキャッシュ（30 秒）、
  外れたら `runtime::offload` の専用スレッドで `getaddrinfo` する。3 つの `connect_str` をこれに置き換えた。
- `clippy.toml` の `disallowed-methods` に `std::net::ToSocketAddrs::to_socket_addrs` を追加し、
  正当な利用（起動時のリスナー作成・ヘルスチェック専用スレッド・offload 内・テスト）だけを理由付きの
  個別 allow にした（再発防止）。
