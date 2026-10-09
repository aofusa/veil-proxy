# B-101: 同時 TLS 接続あたりのメモリが大きく、64MB 以下のコンテナで OOM kill される

**ステータス: 完了（feat/v080-limitations）**

## 事象

`tools/container_security` の `resource_exhaustion`（`SKIP_RESOURCE_EXHAUSTION=0`）の
メモリスイープで、`--memory 64m / 48m / 32m`、`--cpus 0.25`、`nofile=256` のコンテナに
`wrk -t4 -c400`（TLS、静的 52KB）をかけると稼働中に OOM kill（exit 137）される。
96MB 以上では最後まで動く。起動はすべての段で成功する。

## 調査（2026-10-07、`veil:glibc`、`docker/assets/conf.d/config.toml`）

cgroup v2 の `memory.stat` を 2〜5 秒ごとに採取した。

| 条件 | 結果 |
|---|---|
| アイドル | anon 約 10MB |
| 512MB 上限・30 秒 × 8 ラウンド | ラウンド後の anon が 75 → 73 → 71 → 70 → 68 → 69 → 67 → 68MB（**頭打ち。リークではない**） |
| 64MB 上限・20 秒 → 60 秒 → 40 秒 × 2 | 38 → 49 → 49 → 59MB と上限に近づく（今回の単体再現では kill までは至らず） |

file / sock / kernel はいずれも 2MB 以下で、増えているのは anon（ヒープ）だけ。
`nofile=256` なので同時接続は 250 前後に頭打ちになり、**同時 TLS 接続 1 本あたり
約 200〜250KB** を使っている計算になる。負荷が去っても anon は元に戻らない
（アロケータとスレッドローカルのバッファプールが保持する）。

## 見立て

- 接続ごとの読み取りバッファ（`BUF_SIZE` = 64KB）・rustls の送受信バッファ・
  静的配信の読み込みバッファが、キープアライブ中の接続に保持されたままになっている。
- `threads = 0` はホストの論理 CPU 数（`--cpus 0.25` でも 4）だけワーカーを作るため、
  スレッドローカルのプールとアロケータのヒープがワーカー数倍になる。

## 対応方針（feat/v080-limitations で対応中）

1. アイドル（キープアライブ待ち）の接続から読み取りバッファを外し、次のデータが来たときに
   プールから借りる（nginx の `client_header_buffer_size` 相当の小さな初期バッファ）。
2. cgroup の CPU クォータからワーカー数を決める（`threads = 0` のとき
   `cpu.max` を見る）。
3. 対策ごとに `alloc-stats` と本スイープで「接続あたりのバイト数」を測り、交互 A/B で
   スループットに影響しないことを確認する（ホットパス規則）。

## 当面の運用

コンテナのメモリ上限は **同時接続数 × 256KB + 32MB** 程度を目安にする
（250 接続で約 96MB）。上限を小さくしたい場合は `[security] max_concurrent_connections` で同時接続を絞る。

## 改修内容（確定、2026-10-09）

1. **待ってから借りる**: キープアライブ待ちは readiness を待ってから読み取りバッファを借りる（アイドル接続から 64KB を外す）。
2. **プールの遅延確保と上限**: `BUF_POOL` の起動時確保をやめ、保持本数に上限を設ける。
3. **cgroup の CPU 割り当てに合わせたワーカー数**: `threads = 0` のとき `cpu.max`（v2）/ `cpu.cfs_quota_us`（v1）から
   実効 CPU 数を求める（既定動作。後方互換は考慮しない）。
4. 処理中の接続の内訳を `alloc-stats` で計測し、上位を削る。

## 結果（2026-10-09）

実装:

- `wait_next_request`（readiness 待ち）を kqueue 限定から全バックエンドへ広げ、`handle_requests` は
  キープアライブ待ちの前に読み取りバッファを借りない。蓄積バッファ（`accumulated`）は待機前に
  `ACCUM_BUF_POOL` へ返し、初期容量は 8KB（空・一意所有・64KB 以下のものだけ戻す）。
- `BUF_POOL` の起動時確保（2MB）をやめ、保持上限を 128 → 64。HTTP/2 の read_buf プール上限 256 → 64。
- mimalloc の `arena_eager_commit` を 0 に（`.init_array` で `main` より前に設定。環境変数があればそちら優先）。
- 方針 2（cgroup CPU）はコード変更不要だった: `threads = 0` が使う `num_cpus::get()` は cgroup v1/v2 の
  クォータを反映済み（`--cpus 0.25` で `Threads: 1` を確認）。ドキュメントに明記。

計測（ubuntu:24.04 コンテナ、`--cpus 0.25`、nofile 1024、TLS、main と交互）:

| 指標 | main | 本対応 |
|---|---|---|
| アイドル anon | 24〜25MB | 10MB |
| 600 アイドル keep-alive TLS 接続 | 77〜94MB（約 100KB/接続） | 30MB（約 33KB/接続） |
| `wrk -c400` 20 秒中の peak（512MB） | 96〜107MB | 56〜59MB |
| rps（512MB、6 回平均） | 約 752 | 約 749（差なし） |
| 64MB 上限 | 424 アイドル接続で OOM | 600 アイドル + wrk 400 で生存 |
| 48MB 上限 | 194 接続で OOM | 600 アイドルは可、wrk 400 で OOM |
