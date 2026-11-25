# B-497: HttpArena の HTTP/2 の計測で、要求の遅延の最大（結果の `p99_latency` の欄）が計測の長さ（約 5 秒）に張り付く（veil・veil-api とも）

**状態: 完了（2026-11-25、feat/httparena-perf。io_uring で再現し、受理の遅れを直した。2026-11-22 起票。結果の欄が p99 ではなく最大だったことは
2026-11-25 に確かめた。F-317 の HttpArena の結果の調査で見つけた）**

## 事象

HttpArena の公開結果（`site/data/results/*.json`。veil・veil-api は v0.10.1。計測は 5 秒）で、HTTP/2 の全部のテストの
`p99_latency` が veil・veil-api とも 1〜5 秒。nginx・agensio・h2o は 10〜300 ms。

| テスト | veil | veil-api | nginx | agensio |
|---|---|---|---|---|
| baseline-h2-256（`-m 100`） | 4.98 s | 4.98 s | 46.5 ms | 12.0 ms |
| baseline-h2-1024 | 4.96 s | 4.94 s | 151.8 ms | 95.1 ms |
| static-h2-256（`-m 32`） | 1.03 s | 4.98 s | 76.3 ms | 44.0 ms |
| static-h2-1024 | 4.21 s | 4.90 s | 279.5 ms | 208.3 ms |
| baseline-h2c-256 | 4.97 s | 5.00 s | 70.1 ms | 9.3 ms |
| json-h2c-1024 | 5.00 s | 5.00 s | 173.2 ms | 208.8 ms |

### この欄は p99 ではなく最大

- 上の 4 つのエントリの結果は 2026-11-16（HttpArena の a2a61c0）の計測。そのときの HTTP/2 の負荷ツールは Ubuntu の `nghttp2-client`
  の h2load で、百分位を出さない。HttpArena の集計（`scripts/lib/tools/h2load.sh`）は「`time for request:` の行の **max** を
  `p99_lat` に入れる」と書いている（h2load に p99 が無いので最大で代える）。表は最大どうしの比較。
- 2026-11-21（44ba5af）から HttpArena は h2load を nghttp2 1.69.0 から作り、`h2load_parse.py` が本当の p99 と最大（`max_latency`）を
  分けて記録する。2026-11-22 の okena の結果はこの形（baseline-h2-256 は p99 1.42 ms・最大 8.67 ms）。HTTP/3（h2load-h3）は前から
  1.69.0 で、本当の p99（veil の baseline-h3 は 15 ms、veil-api は 2.9 ms）。
- veil-api の baseline-h2-256 の平均は 6.37 ms（30.7M 要求）。遅いのは少数の要求で、1% ではない。

### 分かること

- veil・veil-api では、HTTP/2 の**どのテストでも、計測のほぼ全期間（5 秒近く）返らなかった要求がある**（計測の終わりの直前に
  返っている。終わりまでに返らなかった要求は h2load が数えない）。Wasm の有無に関係なく出るので、HTTP/2 のサーバの共通部分
  （接続・ストリームの処理、reactor）の問題と見ている。h2c（平文）でも出るので TLS ではない。HttpArena のイメージは epoll（`full-container`）。
- 1 本のストリーム（か 1 つの接続）が止まるだけなら、スループットへの影響は小さい（256 接続のうち 1 つなら 0.4%）。点（rps）より、
  実際の利用者の要求が 5 秒止まる（クライアントの時間切れになる）ことが問題。HTTP/1.1・HTTP/3 では出ていない（baseline-4096 の
  gcannon の p99 は veil 635 ms・veil-api 2.2 ms）。
- 次の HttpArena の計測（h2load 1.69.0）では p99 と最大が別に出る。p99 は良くても、直さなければ最大は約 5 秒のまま残る見込み。
- ノート PC（4 CPU、main の io_uring ビルド、veil-load の h2c 16 接続 × 100 ストリーム）では、veil-load の p99 は 8〜45 ms（最大は未確認）。

## 調査の手順

1. 同じ条件で再現する: aofish の Docker で HttpArena の h2load（今の `docker/h2load.Dockerfile`、nghttp2 1.69.0）を作り、epoll のビルド
   （`--features full,epoll` か公開イメージ）と io_uring のビルドへ `-c 256 -m 100 -t 16 -D 5` を当てる。HttpArena と同じ要求
   （`/baseline2?a=1&b=1`、h2c は `-p h2c`、静的は 20 ファイル・`Accept-Encoding: br;q=1, gzip;q=0.8`）。最大と p99 を見て、
   h2load の `--log-file`（要求ごとの記録）で遅い要求がどの接続・いつ送ったものかを見る。ワーカー数も変える。
2. 止まっている要求の特定: 接続ごと止まっているのか（その接続の全ストリームが遅い）、1 本のストリームだけか。デバッグのビルドで、
   1 秒以上進んでいないストリーム・接続の状態（待っているもの・ストリーム数・`write_buf` の量・フロー制御の窓）を出す。
3. 候補:
   - 接続の集約（F-303。`[performance] concentrate_connections` の既定は Linux の TCP で有効）: 振り分けの CBPF の差し替え・
     動かす接続（`take_shed`）の途中で、受理されないまま・どのワーカーにも poll されないままの接続ができる。
     `concentrate_connections = false` で比べる。
   - reactor の予算（`COOP_BUDGET`）で区切った接続の再投入の漏れ（読み取りが残っているのに起床が来ない。B-332）。
   - HTTP/2 の送出の順: 多数のストリームの応答があるときに、特定のストリームの HEADERS・DATA が後回しにされ続ける（`-m 100`）。
   - 計測の終わりに返る理由（h2load が送るのをやめて他のストリームが空いた・接続を閉じ始めた）から、何を待っていたかを逆算する。

## 再現（aofish、2026-11-25。F-317 の「計測の記録」と同じ環境。f322 のビルド、ワーカー 8 本、h2load `-t 8 -D 5`）

| 負荷（HttpArena と同じ要求） | epoll の最大 | io_uring の最大 |
|---|---|---|
| h2（TLS）256 接続 × 100 | 0.20〜0.23 s | 0.10 s |
| h2（TLS）1024 接続 × 100 | 0.51 s | **h2load が終わらない**（2 回とも。60 秒で打ち切り） |
| h2c 256 接続 × 100 | 0.20〜0.23 s | 0.22〜0.44 s |
| h2c 1024 接続 × 100 | 0.62〜0.70 s | **4.98 s**（最初の応答まで平均 0.94〜1.34 s・最大 5.0 s。0 要求で終わる接続がある） |
| 静的 h2 256・1024 接続 × 32 | 0.08・0.20 s | 0.04・0.13 s |

- epoll は今のコードでは数百 ms に収まる（HttpArena の v0.10.1 の epoll の約 5 秒は、その後の B-332〈reactor で速い相手の接続が
  他を飢えさせる〉で直ったと見ている。v0.10.1 は接続の集約〈F-303〉を持たない）。
- io_uring は 1024 接続で止まる接続がある。カーネルの受理待ちの列の溢れ（`ListenOverflows`・`ListenDrops`）は 0。
- `[performance] concentrate_connections = false` にすると io_uring の h2c 1024 接続の最大は 0.42 s（最初の応答の最大 0.42 s）。

## 原因

- io_uring の受理（`IORING_OP_ACCEPT`）は実行器の 1 周回に 1 件しか終わらない（受理のループは 1 件受理すると次の `Accept` を
  作り、その SQE は次の park で提出される）。reactor は F-155 で 1 周回に 32 件（`accept_batch`）受理する。
- 接続の集約（F-303）は新しい接続を先頭の N 本のワーカーへ振り分ける。h2load が計測の初めに張る 1024 接続は、少数のワーカーの
  受理待ちの列に入る。そのワーカーは既に受理した接続の要求（100 ストリーム）で 1 周回が数 ms になり、1 周回に 1 件ずつの受理では
  最後の接続の受理（とその最初の応答）が数秒遅れる。TLS の h2 は、計測の終わりまでにハンドシェイクが終わらなかった接続を
  h2load が待ち続けて終わらなかった。

## 直し方

- io_uring の `TcpListener` にも `accept_batch` を足した（リスナーは `O_NONBLOCK` なので、`Accept` が終わった直後に `accept4(2)` を
  `EAGAIN` まで、合わせて 32 件。待たない）。HTTP のワーカーと L4 のリスナーの受理のループは両バックエンドで使う。
- 接続の集約は変えない（受理が追いつけば、集約したワーカーでも最初の応答は 1 周回の長さで返る）。

## 試験

- `tests/standalone/accept_burst_tests.rs`（`b497_burst_of_connections_is_accepted_while_busy`）: ワーカー 1 本を HTTP/1.1 の
  パイプライン（48 接続 × 16 本）で忙しくした上で 300 接続を一度に張り、全部の最初の応答までの時間を見る。上限は忙しい負荷の
  1 往復の 40 倍（機械の速さによらない）。ノート PC のデバッグビルドで、直す前の io_uring は 12.1〜12.2 秒（上限 2.9 秒）で落ち、
  直した後は io_uring 0.64〜0.68 秒・epoll 0.24〜0.28 秒。
- `runtime::uring::tcp` の単体試験 `test_b497_accept_batch_takes_the_backlog_without_waiting`（40 接続を 31 件と 9 件に分けて
  受理し、空なら待たずに 0。相手のアドレス）。

## aofish での確認（2026-11-25、81aaa3cf のビルド。上の再現と同じ条件・交互 2 巡、エラー 0）

| 負荷 | io_uring の最大 | epoll の最大 |
|---|---|---|
| h2（TLS）256 接続 × 100 | 83〜93 ms | 198〜206 ms |
| h2（TLS）1024 接続 × 100 | 319〜331 ms（直す前は h2load が終わらない） | 516〜532 ms |
| h2c 256 接続 × 100 | 152〜205 ms | 205〜255 ms |
| h2c 1024 接続 × 100 | 522〜544 ms（直す前は 4.98 s） | 628〜634 ms |
| 静的 h2 1024 接続 × 32 | 130〜134 ms | 189〜201 ms |

- 接続の集約は有効のまま（既定）。io_uring の 1024 接続の最大は epoll より小さくなった。平均は接続数 ÷ スループットで決まる
  （1024 接続で 250〜290 ms）。

## 受け入れ条件

- 上の条件で最大の遅延が nginx と同程度（数百 ms 以下）になる。原因と直し方を本チケットに書く。
- 直した経路の試験（止まり方を再現する単体・E2E）を足す。epoll と io_uring の両方。

## 関連

- [F-317](../features/F-317-httparena-infra-score-plan.md)（親）・[F-323](../features/F-323-h2-per-stream-fixed-cost.md)・F-303・B-332・F-298・F-300
