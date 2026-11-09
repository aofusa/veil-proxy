# B-355: 計測スクリプトのクライアントの打ち切りが効かず、スイートが止まり続ける

**状態: 修正済み（feat/v0120-perf、2026-11-09）**

[B-354](B-354-offload-drain-loses-queued-poll.md) で h2load が止まったとき、`CLIENT_TIMEOUT` の打ち切り（既定 180 秒）が効かず、スイートが 13 時間止まり続けて見つけた。

## 事象

`tools/perf/run_perf.sh` の `run_client` は `timeout "$CLIENT_TIMEOUT" docker run --rm --name "$name" ...`。時間を超えても `timeout` が戻らない。

## 原因

`docker run` の CLI は受けたシグナルをコンテナの PID 1 へ転送して（`--sig-proxy` の既定）、終了を待ち続ける。PID 1 の h2load は SIGTERM のハンドラが無く、
カーネルが PID 1 への既定動作のシグナルを捨てるので、誰も終わらない。`--sig-proxy=false` でも CLI は SIGTERM では終わらない（手元の docker で確かめた）。

## 対応

`timeout -k 10`（10 秒後に CLI を SIGKILL）。戻り値 124・137 のどちらでも `docker rm -f` して打ち切りとして扱う（CLI を殺してもコンテナは動き続けるので、`docker rm -f` が要る）。

## テスト

aofish の Docker で、`sleep` を PID 1 にしたコンテナに対して確かめた（`CLIENT_TIMEOUT=3`）。修正前の形は 15 秒待っても戻らず（外側の `timeout -s KILL` で止めた）コンテナが動き続け、修正後の `run_client` は 13 秒で
戻り値 137 になりコンテナも消えた。
