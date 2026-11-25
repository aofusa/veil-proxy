# F-326: reactor（epoll・kqueue・WSAPoll）の送出を周回の終わりにまとめ、相手の起床を束ねる

**状態: 未着手（2026-11-26 起票。F-319 の計測から分けた。F-317 の子）**

## 今の状態（測った値）

aofish（F-317 の「計測の記録」と同じ条件。サーバは CPU 0-7・ワーカー 8 本、負荷は veil-load・512 接続の HTTP/1.1）の 1 要求あたりの
サーバの CPU（ユーザー + システム時間、µs）。`--rate` で負荷を一定のレートにしたものと、上限の負荷のもの:

| 負荷 | epoll | io_uring |
|---|---|---|
| h1-plain 上限（約 78 万 rps） | 2.2 + 7.2 = 9.4 | 2.2 + 7.3 = 9.5 |
| h1-plain 24 万 rps | 5.6 + 19.5 = 25.1 | 6.2 + 20.0 = 26.2 |
| h1-plain 12 万 rps | 7.8 + 29.4 = 37.2 | 5.7 + 24.0 = 29.7 |
| h1-wasm 上限（epoll 26 万・io_uring 41 万 rps） | 11.7 + 16.5 = 28.2 | 8.1 + 9.9 = 18.0 |
| h1-wasm 20 万 rps | 15.5 + 20.8 = 36.3 | 15.3 + 20.0 = 35.3 |

- 1 要求の費用は負荷が上がるほど下がる（1 回の起床で処理する要求が増える）。同じレートなら両バックエンドはほぼ同じ。
- io_uring は 1 周回の送出を 1 回の `io_uring_enter` で出す（F-300）。応答がまとまって出るので、相手（負荷ツールのスレッド）が眠って
  いても起こすのは束の最初の 1 回で済み、負荷が上がるほど費用が下がって 41 万 rps まで届く。
- epoll は応答ごとに `sendto` する（strace で要求あたり `sendto` 1.01・`read` 1.02・`epoll_wait` 0.03）。応答の間に Wasm の実行
  （数 µs）が挟まると、相手は前の応答を読み終えて眠っており、`sendto` のたびに起こす。ループバックでは送り手が相手の起床の費用を
  システム時間として払う（F-295）。h1-wasm は 26 万 rps で 8 コアを使い切り、io_uring より 1 要求 10 µs 多い。
- コンテナのイメージ（`full-container`）は epoll なので、HttpArena の veil のエントリ（Proxy-Wasm の版）は epoll で動いている。
  BSD・macOS・Windows も reactor。

## 改修内容（案。測ってから）

1. reactor の TCP の書き込みの Future は、最初の試行でソケットへ書かずに、ワーカーの送出の待ち行列へ（fd・バッファ）を積んで
   `Pending` を返す。実行器は park の前（`epoll_wait` の前）と `EVENT_INTERVAL` ごとに待ち行列の分を続けて `sendto`/`writev` し、
   書けた Future を起こす。書き切れなかった分は今と同じく書ける状態を待つ。
   - io_uring の SQE を park でまとめて提出するのと同じ形（F-300）。応答の送出が周回の終わりへ寄るので、相手の起床が束になる。
   - 待ち行列に積んだ Future を drop したら、積んだ項目を取り消す（fd を閉じる前。B-337 の io_uring と同じ不変条件）。
2. 先に費用の上限を測る: 応答を書く経路（HTTP/1.1 の `write_or_hold` の送出・HTTP/2 の `write_buf` の flush）だけを、周回の終わりに
   まとめて呼ぶ一時的なビルドで、h1-wasm・h1-plain・h2c を上限と一定レートで測る（環境変数で切り替えて同じバイナリで比べる。B-339）。
3. 1 要求に 1 回 Future を起こし直す費用（タスクの poll 1 回）と、応答が周回の終わりまで遅れる分の遅延（p99）も見る。

## 受け入れ条件

- aofish の epoll の h1-wasm（上限の負荷）で 1 要求の CPU が 15% 以上下がる。h1-plain・h2c・パイプラインは悪化しない（±3%）。
  p99 の遅延が悪化しない。
- reactor の全プラットフォーム（epoll・kqueue・WSAPoll）でビルドと E2E（epoll の E2E・自己完結型 E2E、BSD・macOS・Windows は
  可能な範囲で実機・VM。AGENTS.md: Linux の既定ビルドは reactor を 1 行もコンパイルしない）。

## 期待する効果

- HttpArena の veil（Proxy-Wasm）のエントリが epoll のままでも、h1 の Wasm の経路が io_uring に近づく（26 万 → 35 万 rps 前後の試算）。
  コンテナの既定の seccomp（io_uring を拒否する）で動かす利用者にも効く。

## 関連

- [F-317](F-317-httparena-infra-score-plan.md)（親）・[F-319](F-319-wasm-per-request-fixed-cost.md)・[F-320](F-320-httparena-veil-entry-config.md)・
  F-295・F-300・F-322・B-332・B-337
