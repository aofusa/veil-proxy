# B-475: io_uring の accept のループが 1 秒の区切りで受理の要求を取り消し、行き違ってカーネルが受理した接続を閉じていた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max の確認で、io_uring の自己完結型 E2E（`b181_accept_recovers_after_running_out_of_file_descriptors`・
`b122` の試験）がまれに接続のリセットで落ちたことから見つけた（main の 92197f25 から。accept のループの形は 2025-12 から
変わっていない）。

## 事象

- HTTP のワーカー（`listener_workers::spawn_workers`）・L4 の 2 つのループ（`l4::server`）・メッシュ（`mesh::server::accept_loop`）
  は、停止の確認のために `timeout(1 秒, listener.accept())` で待ちを区切り、満了すると `Accept` を落としていた。
- io_uring の `Accept` は落とされると ASYNC_CANCEL を出す。取り消しの前にカーネルが受理を終えていた（完了がまだ読まれて
  いない・接続の到着で起床の処理が既に積まれている・未提出の ACCEPT が取り消しと一緒に提出されてバックログの接続をその場で
  受理する）ときは、後始末がその fd を閉じる（fd を漏らさないための意図した動き）。
- このため、ワーカーが 1 秒受理しなかった後の区切りと行き違って来た接続は、TCP の接続は張れたのに応答無しで閉じられた
  （クライアントには EOF、要求を送っていればリセット）。新しい接続の少ない負荷（キープアライブ・HTTP/2 の長い接続）では
  ワーカーの待ちが毎秒区切られる。
- スレッド 1 本の veil を起動して、接続して 1.0 秒待ってから要求を送り（区切りと揃う）、応答の直後に次の接続で要求を送る
  試験で、640 回中 3 回、次の接続が応答無しで閉じられた（debug のログにその接続の記録が無く、veil は動き続けていた）。
- reactor（epoll・kqueue・WSAPoll）は poll の中で accept(2) するので起きない。

## 対応

- 4 つのループで、`Accept` をループの前に 1 つ作って区切りをまたいで持ち越し、受理が終わったときだけ次の `Accept` を作る
  ようにした（`pin!`。HTTP のワーカーは停止の後に排出の前で落とすので `Box::pin`。ワーカーにつき 1 回の確保）。
  停止の後に受理の要求を残さないのは従来と同じ。

## テスト

- `runtime::uring::tcp::tests::test_b475_accept_kept_across_a_timeout_delivers_the_connection`: timeout と同じ順に poll し、
  区切りの枝が勝つ直前に接続が来ても、同じ `Accept` を poll し続ければ受け取れる。
- `runtime::uring::tcp::tests::test_b475_dropping_an_in_flight_accept_closes_the_raced_connection`: 同じ場面で `Accept` を
  落とすと（修正前のループ）、その接続は後始末に閉じられ、次の受理には来ない。
- 上の起動直後の試験を修正後のビルドで 1,200 回（2,400 要求）回し、応答無しで閉じられた接続が無いことを確かめた
  （修正前の率なら 5〜6 回は起きる数）。
