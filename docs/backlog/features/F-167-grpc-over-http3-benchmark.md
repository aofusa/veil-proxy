# F-167: gRPC over HTTP/3 の計測を可能にする

## 背景

`tools/perf/run_perf.sh` の `grpc_h3*` 構成は、**計測クライアントが無い**という理由で
`NA` を emit するフェイルセーフのままだった（k6 の gRPC モジュールは HTTP/2 専用で
QUIC/HTTP-3 上の gRPC を話せない）。結果として veil の **gRPC over HTTP/3 データプレーンは
一度もベンチマークされていなかった**。

## 方針

gRPC は「HTTP の上の単純なフレーミング規約」なので、unary 呼び出し 1 回は次で成立する。

```
POST /<package>.<Service>/<Method>
content-type: application/grpc
te: trailers
body = [1 バイト圧縮フラグ][4 バイト長(BE)][protobuf メッセージ]
```

QUIC 対応 h2load（`local/h2load-h3`）は `-d <file>` でリクエストボディを送れるため、
grpcbin の `hello.HelloService/SayHello`（引数 `HelloRequest{greeting:"veil"}`）に相当する
**11 バイト**のボディ（`00 00 00 00 06 0a 04 "veil"`）を渡せば、veil の
「H3 受信 → h2c 上流への中継 → トレイラー返却」経路をそのまま計測できる。

## 実装

`run_perf.sh` の `run_grpc_h3` を、NA フェイルセーフから h2load ベースの実計測へ置き換える
（ボディは計測時に `printf` で生成し、`$LOGDIR` を read-only マウントして `-d` に渡す）。

## 注意（レポートに必ず書くこと）

**k6 版（`grpc` 行）の rps と h2load 版（`grpc_h3` 行）の rps を直接比較してはならない。**
k6 は VU ベース（1 VU 1 リクエスト直列）、h2load は `-c/-m` の多重化ベースで負荷モデルが違う。
比較して意味があるのは「同じ h2load 条件での veil ビルド間・構成間の相対値」である。

## この計測で判明したこと

初回計測で **5xx が 6.9%** 出た。原因は veil 側の実バグ（B-74: HTTP/3 → h2c 上流に
コネクションプールが無く、リクエストごとに新規 TCP 接続してエフェメラルポートを
枯渇させる = `EADDRNOTAVAIL`）。**計測手段が無い経路にはバグが residence する**という典型例。
