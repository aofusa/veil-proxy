# B-497: HttpArena の HTTP/2 の計測で p99 が計測の長さ（約 5 秒）に張り付く（veil・veil-api とも）

**状態: 未着手・要再現（2026-11-22 起票。F-317 の HttpArena の結果の調査で見つけた）**

## 事象

HttpArena の公開結果（`site/data/results/*.json`、v0.10.1 のイメージ。計測は 5 秒、h2load）で、HTTP/2 のプロファイルの p99 が
veil・veil-api とも 4.9〜5.0 秒。nginx・agensio・h2o は同じプロファイルで 0.01〜0.3 秒。

| テスト | veil p99 | veil-api p99 | nginx p99 | agensio p99 |
|---|---|---|---|---|
| baseline-h2-256（`-m 100`） | 4.98 s | 4.98 s | 46.5 ms | 12.0 ms |
| baseline-h2-1024 | 4.96 s | 4.94 s | 151.8 ms | 95.1 ms |
| static-h2-256（`-m 32`） | 1.03 s | 4.98 s | 76.3 ms | 44.0 ms |
| static-h2-1024 | 4.21 s | 4.90 s | 279.5 ms | 208.3 ms |
| baseline-h2c-256 | 4.97 s | 5.00 s | 70.1 ms | 9.3 ms |
| json-h2c-1024 | 5.00 s | 5.00 s | 173.2 ms | 208.8 ms |

- 1% 以上の要求が計測の終わりまで返っていない（またはほぼ全期間待たされている）。HTTP/1.1・HTTP/3 の p99 は秒に届かない
  （baseline-4096 は 635 ms・2.2 ms、baseline-h3 は 15 ms・2.9 ms）。
- Wasm の有無（veil はフィルタ、veil-api はハンドラ）に関係なく出るので、HTTP/2 のサーバの共通部分（接続・ストリームの処理、
  reactor）の問題と見ている。HttpArena のイメージは epoll（`full-container`）。
- ノート PC（4 CPU、main の io_uring ビルド、veil-load の h2c 16 接続 × 100 ストリーム）では再現していない（p99 8〜45 ms）。

## 影響

- スループットにも効く可能性がある: h2load はストリームの枠（`-m`）が埋まると次を送らないので、止まったストリームが接続の並列度を減らす。
- HttpArena では p99 は点に入らないが、結果の表に載る。

## 調査の手順（案）

1. 同じ条件で再現する: epoll のビルド（`--features full,epoll` か公開イメージ）、h2load `-c 256/1024 -m 100 -t <多く> -D 5`、
   HttpArena と同じ要求（`/baseline2?a=1&b=1`、静的は 20 ファイル・`Accept-Encoding: br;q=1, gzip;q=0.8`）。aofish（16 スレッド）で、
   ワーカー数を変えて出方を見る。io_uring のビルドでも出るか。
2. 止まっているストリームの特定: 応答の送出待ち（送信バッファ・フロー制御の窓）、要求の受信待ち（WINDOW_UPDATE の出し忘れ）、
   タスクの起床待ち（reactor の `COOP_BUDGET` で区切った後の再投入・`EVENT_INTERVAL`、B-332）のどれか。デバッグのビルドで
   1 秒以上開いているストリームの状態を出す。
3. 候補:
   - 接続の集約（F-303。`[performance] concentrate_connections` の既定は Linux の TCP で有効）: h2load は計測の最初に全接続を張り、
     接続は計測の間ずっと同じワーカーに残る。最初の振り分けが一部のワーカーに偏ると、その接続のストリームが遅れ続ける。
     `concentrate_connections = false` で比べる。
   - reactor の予算で区切った接続の再投入の漏れ（読み取りが残っているのに起床が来ない）。
   - 接続のフロー制御の窓（静的配信は 1 応答が数十 KB で、`-m 32` × 接続数）。

## 受け入れ条件

- 上の条件で p99 が nginx と同程度（数百 ms 以下）になる。原因と直し方を本チケットに書く。

## 関連

- [F-317](../features/F-317-httparena-infra-score-plan.md)（親）・F-303・B-332・F-298・F-300
