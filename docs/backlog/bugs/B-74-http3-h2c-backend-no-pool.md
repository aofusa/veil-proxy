# B-74: HTTP/3 → h2c 上流にコネクションプールが無く、負荷時に `EADDRNOTAVAIL` で 5xx になる

## 事象

gRPC over HTTP/3（F-167 で計測可能になった経路）に h2load
（`--alpn-list=h3 -n 30000 -c 100 -m 10`）で負荷をかけると、
**30,000 リクエスト中 1,769 件（5.9%）が 5xx** になり、スループットも
短時間実行（`-n 10000`、3.6 秒）の **2,799 rps** に対し
長時間実行（`-n 30000`、41 秒）では **726 rps** まで落ちる。

veil のログ:

```
[HTTP/3] H2C backend connect error: Cannot assign requested address (os error 99)   × 1769
[HTTP/3] Async backend proxy error: Cannot assign requested address (os error 99)   × 1769
```

## 原因

`src/http3_server.rs::proxy_to_h2c_backend_async` が **リクエストごとに**
`TcpStream::connect_str()` + `H2cClient::handshake()` を行い、応答後に接続を捨てている。
HTTP/2 側の同等経路（`proxy.rs::h2_proxy_h2c`）は F-106 で導入した
スレッドローカルの `H2C_POOL` を使って接続を再利用しているのに対し、
**HTTP/3 経路だけがプールを通っていない**。

その結果、毎リクエストで TIME_WAIT のエフェメラルポートが積み上がり、
数万リクエストでポートを枯渇させて `connect(2)` が `EADDRNOTAVAIL` を返す
（B-44 と同じ表面化の仕方）。加えて 1 リクエストごとに TCP ハンドシェイク +
HTTP/2 プリフェース/SETTINGS 往復のコストを払っている。

## 改修案

`h2_proxy_h2c` と同じ形にする。

1. `crate::pool::H2C_POOL` から `get(addr)`、無ければ接続 + ハンドシェイク。
2. 応答完了後、`is_reusable()` なら `put(addr, client, max_idle, idle_timeout)` で返却。
3. プールから取り出した接続での送信が失敗したら、**新規接続で 1 回だけ再試行**する
   （プール内の接続が上流に切られていた場合の救済。`h2_proxy_h2c` と同じ）。
4. `format!("{}:{}", host, port)`（リクエストごとの `String` 確保）を
   `http_utils::HostPortStr`（スタック）へ置換する。

**注意**: HTTP/3 のワーカースレッドと HTTP/2 のワーカースレッドは別スレッドであり、
`H2C_POOL` はスレッドローカルなので相互に干渉しない（同一スレッド内での再利用のみ）。
