# F-158: HTTP/2 per-stream タスクのインライン初回 poll

**優先度**: P2
**ステータス**: 完了（FreeBSD 実測で確認、Linux 非劣化確認は別途）
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
