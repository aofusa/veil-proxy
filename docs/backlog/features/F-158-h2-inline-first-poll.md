# F-158: HTTP/2 per-stream タスクのインライン初回 poll

**優先度**: P2
**ステータス**: 完了（**readiness reactor 限定で採用**。io_uring では実測で退行したため不採用）
**関連**: F-116（per-stream タスク多重化）、F-157（残件として本件を指名）、F-141/F-155（kqueue readiness ヒント）

---

## 背景

F-157 は FreeBSD の `h2c_file_plain`（平文 HTTP/2 静的配信）が対 nginx 0.80〜0.89 に
留まる残件を次のように診断し、クローズしていた:

> 残差は HTTP/2 の per-request タスク spawn + チャネル + `Notify` 起床の固定費と
> 見られる（`poll` が 0.53/req 残っているのもここ）。

すなわち **1 リクエストあたりの固定費**であって、バイト転送コストではない。

## 事象（改修前の構造）

静的キャッシュヒット時の 1 リクエストは次を通る:

```
process_frame → h2_spawn_for_request → TaskPool::spawn（スラブ登録 + ready キュー投入）
  → メインループへ戻る → drive_h2_streams（resp_rx は空。タスクはまだ実行されていない）
  → flush（なし） → try_read_frame_buffered（なし） → processed_any = false
  → h2_select_readable_or_notify   ← ここで初めてエグゼキュータへ制御が渡る
  → タスク実行（キャッシュヒットなので待ちなしで完了）→ notify
  → メインループ再開 → drive_h2_streams が Head + Body を送出
```

タスク本体（`h2_request_task`）は、静的キャッシュが両方ヒットしていれば
**実際に pend する await を 1 つも持たない**（offload ゼロ経路 + 容量 4 のチャネルへ
2 メッセージ）。にもかかわらず **エグゼキュータへの往復が 1 回強制されている**。

## 改修内容

**`TaskPool::spawn_inline`**: future をプールのスロットへ格納したうえで、
**そのタスク自身の実 Waker** で 1 回だけその場で poll する。

- `Poll::Ready` → エグゼキュータの ready キューへ一度も積まれずに完了・解放。
  上記の強制往復（および付随する `poll(2)`・`Notify` 起床）が丸ごと消える。
- `Poll::Pending` → 通常の `spawn()` が作る状態と完全に同一（スロットに body を戻し、
  generation 据え置き、以降の wake で ready キューへ）。

`src/runtime/uring/executor.rs` と `src/runtime/reactor/executor.rs` の**両方**に
対称実装（`spawn_body_and_poll`）。呼び出し元は `proxy.rs` の `h2_task_spawner` のみ。

副次: `send_headers*` を `AsRef<[u8]>` でジェネリクス化し、
`drive_h2_streams` の `Vec<(&[u8], &[u8])>` の `collect()`（1 レスポンス 1 確保）を除去。

### 健全性

1. **Waker**: 初回 poll に noop waker を使うと Pending 時に起床不能になる。
   通常の `run_ready_tasks` と同一の `make_waker(index, generation)` を使うため、
   Pending 時の起床可能性は通常 spawn と同一。
2. **自己 wake**: poll 中に wake された場合 `schedule` が ready へ積む。Pending なら
   そのまま再開、Ready なら generation +1 で stale エントリは世代不一致で無視される
   （既存の `stale_waker_is_ignored` と同じ機構）。
3. **借用規律**: poll 中は `EXEC_STATE` を借用しない（future が再度 spawn しても安全）。
4. **送出順序**: インライン完了により、タスクが `resp_tx` を drop するのが
   `streams.insert` より前になり得る。`stream_channel::try_recv` は
   **キューを pop してから `sender_closed` を見る**ため、バッファ済みの Head/Body は
   失われない（この不変条件が崩れると全リクエストが RST_STREAM になる）。

### 採用しなかった案

`docs/artifacts/h2c_and_h2c_perf_optimization_spec.md` が提案していた
「静的キャッシュヒット時にルーティングとキャッシュ参照をフレームループへインライン展開する」案は
**不採用**。`check_security`・WASM フィルタ・圧縮・管理 API・Prometheus・アクセスログを
バイパスする第 2 の経路を作ることになり、セキュリティ検査の二重化は将来の齟齬を生む。
本改修は**ロジックを一切複製しない**。

同仕様書の「`ReadableFd::poll` の `libc::poll` を消す」案も**不採用**。
AGENTS.md（F-141/F-155）が「ヒントが無いときの `poll(2)` フォールバックを消してはならない」と
実測に基づいて明記しており、消すと kqueue 往復 1 回分のレイテンシが乗る。
**`poll(2)` が残っていること自体が問題なのではなく、そこへ到達する回数が問題**だった。

## 実測（FreeBSD 14.3 aarch64 / QEMU+HVF、交互 A/B 10 ラウンド）

`/root/ab/{base,new}/veil`（basename は `veil` のまま = F-156 の教訓）、
md5 で 2 バイナリが別物であることを確認済み。

**3B（リクエスト単価）**

| | 中央値 | 最小 | 最大 |
|---|---|---|---|
| base | 555,119 | 441,871 | **581,694** |
| new | **706,779** | **652,805** | 727,641 |

**中央値 +27.3%、10/10 勝、分布は完全に分離**（base の最大 < new の最小）。

**54,576B（バイト単価）**: 96,981 → 97,794（**+0.8%、8/10**）＝ 退行なし。

固定費を消した改修なので **3B で大きく効き 54KB では埋もれる**のが期待される挙動であり、
実測はそれと一致する。

## 【重要】io_uring では退行した → reactor 限定に変更した

FreeBSD で +27.3% を確認したあと Linux（io_uring・既定バックエンド）でも交互 A/B を
取ったところ、**逆に退行していた**。

| バックエンド | 構成 | base 中央値 | new 中央値 | 差 | base の勝ち |
|---|---|---|---|---|---|
| kqueue（FreeBSD aarch64） | h2c 3B | 555,119 | **706,779** | **+27.3%** | 0/10 |
| kqueue（FreeBSD aarch64） | h2c 54KB | 96,981 | 97,794 | +0.8% | 2/10 |
| **io_uring（Linux x86_64）** | `h2c_file` | 9,678.9 | 9,231.8 | **−4.6%** | **11/12** |
| **io_uring（Linux x86_64）** | `h2c_proxy` | 2,744.1 | 2,639.1 | −3.8% | 5/6 |

（Linux は 12 ラウンド + 6 ラウンドの 2 回、いずれも交互・順序入替。24 ラウンド中 21 で
ベースラインが勝った。）

### 原因: 消せる往復コストがバックエンドで違う

- **reactor**: 待機は `h2_select_readable_or_notify` → `Readable::poll` を通り、
  kqueue ヒントが無いときは**同期 `poll(2)` を 1 回発行する**
  （この `poll(2)` フォールバック自体は F-141/F-155 の実測により削除禁止）。
  インライン初回 poll はこの往復ごと消せるので大きく効く。
- **io_uring**: 同じ待機が `IORING_OP_POLL_ADD` の SQE 1 本で、他の SQE とまとめて
  submit される。**消せる往復コストがそもそも小さい**。残るのはインライン poll 側の
  固定費（スロット確保・Waker 構築・`EXEC_STATE` の追加借用）と SQE バッチングの
  乱れだけになり、差し引きで退行する。

### 対処

`veil_rt_reactor` のときだけ `spawn_inline` を呼ぶよう `h2_task_spawner` を cfg 分岐し、
**`src/runtime/uring/executor.rs` は F-158 以前と 1 バイトも変わらない状態へ戻した**
（`git diff` で確認済み）。AGENTS.md の「io_uring パスのロジックは変えない」と
「Linux io_uring 経路の非劣化保証」の両方を満たす。
`spawn_inline`/`spawn_body_and_poll` は reactor 側にのみ存在する（dead_code なし）。

適用対象: FreeBSD / OpenBSD / NetBSD / macOS / Linux `--features epoll`。
**epoll も同じ恩恵を受けるはず**である（readiness ヒントは kqueue 限定の実装なので、
epoll は常に `poll(2)` フォールバックを通る）。

### 教訓

**「片方のプラットフォームで大きく効いた」ことは、もう一方で効くことを何ら意味しない。**
本件は同一の変更が **+27% と −4.6%** に分かれた。仕様書は FreeBSD の h2c と
Linux の h2c proxy を 1 つの問題として扱い共通の改修案を並べていたが、
**待機プリミティブが違う以上、両者は別の問題である**。
新しいホットパス最適化は**必ず両バックエンドで交互 A/B を取ること**。

## 計測時の落とし穴（本件で踏んだもの）

1. **`tools/qemu/bsd-vm.sh <os> <arch> ssh` は標準入力を転送しない。**
   `tar czf - src | bsd-vm.sh ... ssh "tar xzf -"` は**エラーを出さずに何も展開しない**。
   ベースライン src の push が黙って失敗し、改修後 src のまま「ベースライン」を
   ビルドしていた。VM への流し込みは直接 `ssh -i ~/.ssh/veil_qemu_key -p 2320
   root@127.0.0.1` を使うこと。
2. **`git archive HEAD` はファイル mtime をコミット時刻にする。**
   cargo は mtime で差分判定するため、既存のフィンガープリントより古い mtime で
   展開すると**再ビルドが走らない**。展開後に `find src -name '*.rs' -exec touch {} +`
   すること。

この 2 つにより、一度は `base` と `new` の **md5 が完全一致**した状態で A/B を回しかけた
（＝「差が無い＝退行なし」という誤結論が出るところだった）。
**A/B の前に 2 バイナリの md5 が異なることを必ず確認すること。**

## 検証

- `cargo test --lib --features full`: 879 passed
- `cargo test --test integration_tests --features full`: 54 passed
- E2E（io_uring 既定）: 543 passed / 1 failed（`test_http3_large_request_body`）
  → **単独再実行 3/3 成功**、かつ `spawn_inline` の呼び出し元は
  `h2_task_spawner` のみで **HTTP/3 経路は改修コードを 1 行も実行しない**ため
  本改修とは無関係の環境要因（co-tenant 負荷、load average 1.4〜3.2）。
- E2E（`full,epoll` = reactor）: **544 passed / 0 failed**
- clippy（`full` / `full,epoll`）・`cargo fmt --check`: 警告ゼロ

## 全プラットフォーム検証結果（2026-08-22 最終）

`--profile dist` で **17 成果物すべて**をビルドし直したうえで E2E を実行した。

| プラットフォーム | バックエンド | ビルド | E2E |
|---|---|---|---|
| Linux x86_64（既定） | io_uring | ✅ | ✅ 544/544（※1） |
| Linux x86_64 `--features epoll` | reactor/epoll | ✅ | ✅ 544/544 |
| Linux aarch64 | io_uring | ✅ | — |
| FreeBSD 14.3 aarch64 | reactor/kqueue | ✅ | ✅ ベースラインと同一（B-68 の既存 2 件のみ） |
| FreeBSD 14.3 x86_64 | reactor/kqueue | ✅ | ✅ 543/1（既知フレーキー 1 件） |
| **OpenBSD 7.9 aarch64** | reactor/kqueue | ✅ | ✅ **543 passed / 0 failed** |
| OpenBSD 7.9 x86_64 | reactor/kqueue | ✅ | 538/5（B-71: 既存の HTTP/3 4 件 + フレーキー） |
| NetBSD 10.1 aarch64 | reactor/kqueue | ✅ | B-70（テストヘルパの panic で中断） |
| NetBSD 10.1 x86_64 | reactor/kqueue | ✅ | 540/4（HTTP/3 系のみ） |
| macOS universal2 | reactor/kqueue | ✅ | 実機なし |
| Windows x86_64 / aarch64 | reactor/wsapoll | ✅（B-69 修正後） | 実機なし |

※1: フルスイート並列実行時に `test_http2_request_body_streaming` /
`test_http3_large_request_body` が稀に失敗するが、**いずれも単独実行では 3/3 成功**し、
実行ごとに失敗するテストもバックエンドも入れ替わるため環境フレーキーと確定している
（io_uring は `spawn_inline` を使わないため F-158 と無関係でもある）。

**F-158 に起因する回帰はどのプラットフォームでも検出されなかった。**
検証中に見つかった失敗はすべて、ベースライン実行または単独再実行により
既存問題ないし環境フレーキーであることを実測で確認済み（B-68 / B-70 / B-71）。
