# F-297: コア制限下の遅延（1 要求あたりの CPU を減らす）

親: [F-295](F-295-v0120-perf-harness.md)。公平性は [B-332](../bugs/B-332-reactor-hot-connection-starves-others.md)。

## 現状

HttpArena の latency-500k-8cpu（`GET /baseline11?a=1&b=2`、8 論理 CPU、1024 接続、固定レート 500k、CO 補正）で
372,770 req/s、rate 0.7455、p99 6.2 s。1 要求あたり 21.4 µs の CPU で、8 CPU の容量が固定レートを下回っているので
遅延が積み上がる。上位は 14.7〜16.4 µs/req。つまり **1 要求あたりの CPU を 16 µs 程度へ下げる** のが本筋。

## 再現と分解（aofish の WSL、epoll 版、サーバ CPU 0-7、負荷 CPU 8-15）

- 飽和 378,353 rps（外部とほぼ同じ）。CPU はユーザ 1/3、システム 2/3。
- 1 要求あたりのシステムコール（LD_PRELOAD で数えた。飽和でも 200k の固定レートでも同じ）:

| read | send | poll | epoll_ctl | epoll_wait |
|---|---|---|---|---|
| 1.00 | 1.00 | 2.00 | 1.00 | 0.008〜0.46 |

`poll(2)` 2 本と `epoll_ctl(MOD)` 1 本は、キープアライブの待ちごとに「確認 → 再武装 → 起床後にもう一度確認」している分。

## 仮説

1. 要求ごとに 3 本のシステムコールを消せる（5 本 → 2 本 + `epoll_wait` の按分）。
2. ユーザ時間（約 7 µs）の上位は `handle_requests`・`handle_backend`・memmove・`Backend::clone`・`shared_v_*`。
   ヘッダの扱いと経路の解決で積み上がっている。

## 改修内容

### A. epoll の登録を要求ごとに触らない（AGENTS.md の B-75 の条項を改める）

`EPOLLONESHOT` + 待ちごとの `EPOLL_CTL_MOD` をやめ、**待機者がいる方向だけを level-triggered で武装したまま残す**:

- `register`: 既にその方向が武装済みなら Waker を置くだけ（システムコール無し）。
- `dispatch_event`: 起床させる待機者がいれば起こして武装を残す。待機者がいないのにイベントが来たら、その方向を `MOD` で下ろす
  （レベルで鳴り続けるのを止める）。次に待つときに `MOD` で上げ直すと、カーネルがレベルを再評価する（取りこぼさない）。

レベルトリガなので B-75 が避けた「読めないが再武装もされない」状態は起きない（`EAGAIN` の後の待機でカーネルの状態が
そのまま効く）。ET と違い、`EAGAIN` までの読み切りも要らない。

### B. 確認用の `poll(2)` を外す

- `Readable`・`ReadFuture` の `read` は try-first なので、キープアライブの待ちは「まず待ちに入る」（`readable_lazy`）。
- 待ちから起きた後は確認せずに読みにいく（`read` が `EAGAIN` なら Waker を置いて待つ。A により追加のシステムコールは無い）。
- `Readable` の `poll(2)` は、ヒントの無い汎用の待ち（`select` の中など）に残す。

B-75 のハングは「ヒントで `Ready` を返したが `register` を通らず disarm のまま」が原因の仮説で、A で再武装そのものが
不要になるので前提が消える。kTLS + HTTP/2 の再現手順（`wrk ×3 → h2load`）を回帰の手順に加える。

### C. ユーザ時間

プロファイルの上位から、要求ごとの確保・コピー・クローンを減らす（`Backend::clone`、ヘッダの `to_vec`、タイマーの
登録・解除など）。実測で効いたものだけ入れる。

## 計測

`python3 tools/perf/api_ab.py --scenario latency`。8 CPU に固定し、固定レートを段階的に上げて、達成率と p50/p99/p99.9 を見る。
システムコール数は `tools/perf/syscall_count/`。

## 受け入れ条件

- 1 要求あたりのシステムコールが `read` + `send` + `epoll_wait` の按分になる。
- 8 CPU の飽和スループットが改修前より大きく増え、固定レートでの p99 が下がる。
- epoll の E2E（`VEIL_E2E_FEATURES="full,epoll"`）と kTLS + HTTP/2 の再現手順が通る。

## 依存・リスク

- reactor の登録の意味論を変えるので、offload の eventfd（`wake_all_readers`）・WebSocket の中継・静的配信など
  同じ fd に別の待ち手がいる経路を読み直す。io_uring の経路は変えない。
