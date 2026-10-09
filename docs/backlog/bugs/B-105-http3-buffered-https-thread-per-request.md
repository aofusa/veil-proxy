# B-105: HTTP/3 バッファ経路の HTTPS 上流が要求ごとに OS スレッドを生成する

**状態: 完了（feat/v080-limitations）**

## 事象

`http3_server.rs::proxy_to_tls_backend_async` は、非同期で張った TCP を捨て、`std::thread::spawn` したスレッドで
同期接続 + 同期 TLS（`ClientConfig` とルート証明書ストアを毎回構築）を行い、結果を 5ms 刻みでポーリングする。
しかもバッファ経路はメインループ内で await される（B-97）。

## 影響

Wasm や `buffering = full` を付けた HTTPS 上流ルートでは、1 要求あたり「スレッド生成 + TCP + TLS + 5ms 単位の待ち」を
ワーカー内で直列に処理する。上流が近くても 1 要求 5ms 以上＝ワーカーあたり 200 req/s 以下。

## 改修

HTTP/1.1・HTTP/2 と同じ非同期 `ClientTls` + `HTTPS_POOL` の経路に置き換え、スレッド経路を削除する。

## 結果（2026-10-09）

`proxy_to_tls_backend_async`（要求ごとのスレッド生成 + 同期 TLS + 5ms ポーリング + 毎回のルート証明書ストア構築）を削除し、
バッファ経路の HTTPS 上流も B-104 の非同期プール付き経路（`exchange_buffered`）で処理する。
