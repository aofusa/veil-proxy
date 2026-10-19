# F-240: v0.8.0 の性能の改善（nginx を下回る項目を直す）

**状態: 完了（feat/v080-release、2026-10-18）**

## 目的

v0.8.0 の計測（Linux x86_64 の Docker・FreeBSD 14.3 aarch64）で nginx を下回る項目について、仮説を立てて直す。
退行が無く、nginx 以上になるまで繰り返す。

## 計測の方法の見直し（先に直したもの。B-153）

FreeBSD の計測（`tools/perf/bsd/run_perf_bsd.sh`）は、次の 3 点で veil に不利に出ていた。

- nginx を計測対象のコア（`SRV_CPUS`）に固定していなかった（説明には「固定する」と書いていた）。
- proxy・L4 の上流が計測対象と同じ nginx のプロセスだった（nginx の計測では中継と上流が同じイベントループで済む）。上流を
  別のプロセスにし、`UP_CPUS`（既定は負荷生成と同じコア）に固定した。
- いつも nginx を先に測っていた。VM ではラウンドの中で後に測るほど遅くなるので、反復ごとに順を入れ替えた。

あわせて、起動直後の veil に負荷を当てて全要求が失敗する（0 rps）のを避けるため、HTTP の応答を待ってから測る。
クライアントが律速になる構成（h2c・HTTP/3 の 54KB）は、計測対象 1 コア・負荷生成 3 コアの「サーバ律速」でも測る
（`SRV_CPUS=0 GEN_CPUS=1,2,3 UP_CPUS=1,2,3 SRV_THREADS=1 LOAD_THREADS=3`）。

## 直したもの

| 変更 | 対象 | 効果（FreeBSD aarch64、特記の無いものはサーバ律速の対 nginx 比） |
|---|---|---|
| `[http3] recv_drain_max` の既定 64 → 1024 | reactor の HTTP/3 | 3B 205k → 280k req/s（nginx 266k）。64 だと 1 周回のデータグラムが 32 接続に散り、1 送信の応答が 1.5 個（nginx 3.4 個） |
| reactor のタイマーの待ち時間を切り捨てない（B-152） | reactor 全般 | 1ms 未満の先のタイマーで poller が空回り（HTTP/3 で空の kevent が 50 万回/秒）。TLS 中継 3B の CPU/要求 −5% |
| 本文を一部だけ送れた接続を再投入しない | HTTP/3 | 送れない周回を sleep 0 で回さない |
| QUIC の輻輳制御の既定 bbr → cubic | HTTP/3 | pacing の送出時刻を守らない送出で BBR が受け手で捨てられ再送（54KB 1 つに 61 パケット → 46）。54KB 0.86〜1.05 → 1.03〜1.12、Linux +4〜6% |
| h2c の DATA を共有メモリ + `sendfile(2)`（+ `TCP_NOPUSH`） | FreeBSD の平文 h2c 静的配信 | 54KB 0.92 → 1.33。サーバの CPU/要求 10.2 → 6.2µs（nginx 8.6） |
| HTTP/1.1 の中継の応答ヘッダを 1 本のバッファへ | 全 OS の HTTP/1.1 中継（kTLS 無効時） | 行ごとの `format!`・`Vec` を止めた。TLS 中継 3B の CPU/要求 −17%、rps 1.1〜1.2× |
| 負荷分散の遅延の記録に粗い時計 | 中継 | 要求ごとの `Instant::now()` 2 回を減らす |
| L4: 短い read の後は kevent を待ってから読む・アイドル監視を 1 本のタイマー・2 方向を別タスク | Linux 以外の L4 | read 4.0 → 3.1/req、CPU/要求 −6%。3B 0.90 → 0.98〜1.0 |
| `TCP_NOPUSH` を 128KB 以上の応答だけに | FreeBSD の平文 HTTP/1.1 静的配信 | 54KB で 2 syscall/req 減 |
| HTTP/3 の本文を quiche の送信バッファへコピーせず `Bytes` のまま渡す（`BufFactory` + `send_body_zc`） | HTTP/3 | FreeBSD の h3_file 54KB の CPU/要求 113 → 103µs（−8.6%、nginx 98µs）。memcpy のサンプル 901 → 411。Linux の交互 A/B（h2load、下の時刻と合わせて）は h3_proxy +2.7%・h3_file_cache +1.6% |
| 受信の束・1 接続ぶんの送出で 1 回だけ時刻を読み、quiche に渡す（vendoring の quiche に `set_time_hint`） | HTTP/3 | quiche は recv・send のたびに `Instant::now()` を読んでいた（54KB 1 つで約 60 回）。`__vdso_gettc` のサンプル 330 → 115、3B の CPU/要求 −2〜3% |

quiche の `send_body_zc` には、ストリームの容量で一部しか書けない回に、流量制御で止まっていないのに相手の上限で
STREAM_DATA_BLOCKED を送る不具合があった（バッファの全長を「送りたい量」として渡していた）。ngtcp2 のクライアント
（h2load・curl）はその上限が最終的なサイズより大きいので FINAL_SIZE_ERROR で接続を閉じる。E2E の HTTP/3 の
クライアントは quinn で、これを無視するため試験は通り、Linux の計測（h2load）で 2〜87 req/s になって見つかった。
vendoring の quiche で、送る分に切ったバッファを渡すように直した（`third_party/quiche/README.veil.md` の 3c、
単体試験 `zc_body_send_does_not_signal_false_stream_blocked`）。

結果の全体は `docs/perf/README.md`。

## 関連

F-151・F-152（HTTP/3 の reactor の周回）、F-155・F-157（FreeBSD の静的配信）、F-173（L4 の空打ち）、B-152、B-153。
