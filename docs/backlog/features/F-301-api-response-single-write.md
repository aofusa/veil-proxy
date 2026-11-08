# F-301: API の応答で、大きな本文をヘッダと 1 回の書き込みで送る

**状態: 完了（feat/v0120-perf、2026-11-08）**

親: [F-295](F-295-v0120-perf-harness.md)。要求事項には無いが、[B-339](../bugs/B-339-epoll-level-trigger-sticky-wake-order.md) の計測の途中で見つけた。

## 問題

`api_ab.py --scenario json` の identity の行（epoll）で 1 要求あたりのシステムコールを数えると、`send` が 1.97 本だった
（`read` 0.99、`epoll_wait` 0.04、`epoll_ctl` 0.001）。

HTTP/1.1 のハンドラの応答（`proxy::h1::write_api_response`）は、本文が 3KB（`API_INLINE_BODY_MAX`）以下ならヘッダのバッファへ続けて
1 回で書き（F-202）、超えると本文を 2 回目の `write_all` で送っていた。HttpArena の json の応答（`/json/25`〜`/json/50`）は 3KB を超える。

ループバックではサーバの `send(2)` が相手の受信の処理と起床の費用を払う（[F-295](F-295-v0120-perf-harness.md)）ので、1 つの応答を
2 本に分けると、その費用も 2 回になる。

## 対応

本文が 3KB を超えてストリーミングでない応答は、ヘッダと本文を `ServerTls::write_all_vectored` で送る。

- 平文: `sendmsg(2)` 1 本（io_uring は `IORING_OP_SENDMSG`。既存のオペコード）。本文はコピーしない。
- rustls: 両方を送信キューへ積んでから `flush_tls_writev` を 1 回（`writev(2)` 1 本）。
- kTLS: 従来どおり 2 回（`write_all_vectored` の kTLS の分岐は変えない）。

3KB 以下の本文は従来どおりヘッダのバッファへ写して 1 回で書く。静的配信（メモリキャッシュの応答）とプロキシの応答は既にこの形だった。

## テスト

`tests/api/library_e2e.sh` の rust に「3KB を超える本文を同じ接続で 2 回続けて受け、`Content-Length` どおりの長さで JSON として読めて、
2 回とも同じで、余分なバイトが無い」確認を足した（`/dataset/200`、平文の HTTP/1.1）。既存の圧縮の確認（gzip を復号した中身が
識別の応答と一致する）もこの経路を通る。

## 計測

ビルドごとのコードの配置だけで json の identity の行が 15% 動く（[B-339](../bugs/B-339-epoll-level-trigger-sticky-wake-order.md)）ので、
2 回の書き込みに戻す切り替えを環境変数で入れた一時的なビルドを 1 つ作り、同じバイナリの中で比べた（B-339 の並べ替えも入った
最終候補。`api_ab.py --scenario json` の identity と同じ負荷: 256 接続・`/json/25`〜`/json/50`、5 ラウンドの交互の中央値、aofish の WSL2）。

| バックエンド | 2 回の書き込み | 1 回（F-301） | 比 | p99 | サーバ CPU/要求 | 負荷側 CPU/要求 |
|---|---:|---:|---:|---:|---:|---:|
| epoll | 167,202 | 186,426 | 1.11× | 2.67 → 2.51 ms | 45.0 → 40.4 µs | 20.0 → 16.5 µs |
| io_uring | 177,644 | 237,530 | 1.34× | 2.64 → 1.83 ms | 42.4 → 31.7 µs | 20.7 → 13.3 µs |

1 要求あたりの送信は `send` 1.98 本から `sendmsg` 1.00 本になった。負荷側の CPU も減っている（ヘッダだけ届いた時点で起こされて読み、
本文でもう一度起こされていた分）。io_uring は 2 つの SEND を同じ `io_uring_enter` で出していたが、カーネルの中では 2 回の
`tcp_sendmsg` で、相手の受信と起床も 2 回だった。
