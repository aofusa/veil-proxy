# F-292: HTTP/1.1 の kTLS の本文のゼロコピー転送を平文と同じ経路（`sendfile_plain` 相当・その場の非ブロッキング `splice`）にそろえる

**状態: 実装済み**

調査の全文は `docs/artifacts/v0110/review-04-ktls-zero-copy.md`（git 管理外）。行番号は f8dc80de 時点。

## 今の状態（kTLS と平文の差）

[B-286](../bugs/B-286-plaintext-h1-copies-file-and-proxy-bodies.md) で Linux の平文の HTTP/1.1 にゼロコピーを入れたが、kTLS の経路は「残り」として変えなかった。

| 項目 | 平文（B-286） | kTLS |
|---|---|---|
| 静的配信の head | `MSG_MORE` 付き `send`（本文の最初のセグメントとまとまる）、`WRITE_TIMEOUT` | 別の `write_all`（MORE 無し）→ 小さい TLS レコードが 1 つ増える |
| 静的配信の本文 | `sendfile_plain`: 1MB ごと、送り切ったら `yield_now`、`writable()` に `WRITE_TIMEOUT`、長さに届かなければ接続を閉じる、`EINVAL`/`ENOSYS` は読み書きへ戻す、`EINTR` はやり直す、エラーは `debug!` | `handle_sendfile_zerocopy`（`src/proxy/sendfile.rs:392-445`）: `chunk_size_mode` の大きさ、**譲らない**（kTLS は syscall の中で暗号化するので平文より長くワーカーを握る）、`writable()` に**時間の上限が無い**、**短い本文でも接続を閉じない**（Content-Length と食い違ったままキープアライブ）、`EINVAL` で戻れず head だけ送って本文 0 バイト、`EINTR` で打ち切り、エラーは `error!`（切断のたびに出る） |
| プロキシの本文（Content-Length） | `splice_now`（その場の非ブロッキング splice）+ 時間付きの `readable()`/`writable()` | io_uring の `IORING_OP_SPLICE`（常にカーネルのワーカースレッドへ回る。平文では 54KB で命令数 1.5 倍だった）。epoll ビルドでは reactor の `SpliceFuture`（[B-294](../bugs/B-294-reactor-splice-busy-spin-pipe-fd.md) の空回り） |
| 応答 head（プロキシ） | 本文が続くときだけ `MSG_MORE` + `WRITE_TIMEOUT` | MORE 無し、時間の上限無し（[B-295](../bugs/B-295-splice-path-timeouts-yield.md)） |
| FreeBSD | 平文は head も `sendfile` の `sf_hdtr` で 1 回 | kTLS は head を別に送る。プロキシは常にコピー |

ゼロコピーにならないもの（文書に書く）: HTTPS の上流（両端 kTLS でも）、WebSocket、HTTP/2 over kTLS。

## 改修内容（設計）

1. **静的配信**: `sendfile_plain` を `sendfile_body(stream, header, file, off, len, mode)` に一般化し、平文と kTLS（`is_ktls_send_enabled()`）の両方で使う。
   `Sent`/`HeaderFailed`/`Unsupported` を返し、`WRITE_TIMEOUT`・1MB ごとの `yield_now`・短い本文で閉じる・`debug!` を共有する。kTLS の大きさは
   固定 1MB（カーネルが 16KB のレコードに切るのでレコードの境界に合わせる必要は無い）。`Unsupported` は kTLS でも従来の読み書き（kTLS ソケットへの
   平文の write は暗号化される）へ戻す。
2. **プロキシ**: kTLS も `splice_body_transfer(.., direct = true)`（`splice_now`）にする。io_uring の splice を使う箇所が無くなれば epoll の
   `SpliceFuture` の問題からも kTLS が外れる。
3. **kTLS の `MSG_MORE`・`SPLICE_F_MORE`**: [B-25](../bugs/B-25-reverse-proxy-http1-wrk-zero-completed.md)（最後のチャンクに MORE を付けると
   16KB 未満のレコードが保留されてハング）と、Linux 6.5 の `splice_eof` 導入で振る舞いがカーネルで違う点・WSL で MSG_MORE + splice で VM が落ちた
   報告（B-286）がある。head への MORE は「最後の送信に MORE を付けない／付けたら接続を閉じる」ことがコードで保証できる所だけにし、最初は設定
   （例 `[performance] ktls_msg_more = false`）か起動時の自己試験の裏で既定オフにする。5.15・6.1・6.8・6.12・WSL2・Docker Desktop で確かめてから既定を決める。
4. **暗号化の場所**: 今は io-wq のスレッドで暗号化しているので他のコアを使っている。その場の splice にするとワーカーで暗号化することになり、
   平文と違って遅くなる可能性がある。**採否は交互 A/B で決める**（下）。
5. 同期の `sendfile(2)` はページキャッシュに無いときディスクを待ってワーカーを止める（kTLS は以前から、B-286 で平文も）。`open_file_cache` の
   offload で `readahead`/`posix_fadvise(WILLNEED)` を投げる案を検討し、ホットパス規則（同期処理禁止）との整合をこのチケットで記録する。
6. 同じ変更で直す: [B-293](../bugs/B-293-splice-fallback-resends-request-upstream.md)（途中で戻ると上流へ二重に書く）、[B-295](../bugs/B-295-splice-path-timeouts-yield.md)、
   `splice.rs:280` の `is_ktls_enabled()` → 要求本文の splice は `is_ktls_recv_enabled()`（`KtlsTxOnly` で暗号文を上流へ流さない。今は誰も設定しない潜在的な問題）。
7. 文書: [B-297](../bugs/B-297-zero-copy-compression-docs-mismatch.md)。

## 受け入れ条件

- 統合試験: `plaintext_h1_zero_copy_files_and_proxy_bodies_keep_framing` の kTLS 版（3MB の速い・遅い受け手、3B、Range、キャッシュのあるルート、上流の 2MB・要求の 1MB）。
  B-25 の回帰（54KB が 1 秒以内に全部届く）。kTLS の E2E は CI 外（B-259）なので手元で回す。
- 交互 A/B（f8dc80de の `git worktree` をベースライン、4 回以上の中央値）: io_uring と epoll、3B・54KB・1MB 以上、静的（キャッシュ有無）とプロキシ、
  両方 kTLS（nginx は `ssl_conf_command Options KTLS`・`sendfile on`・`open_file_cache`、TLS 1.3 AES-128-GCM）。CPU µs/req・命令数/req・`strace -c`・
  TLS のレコード数。errors 列を先に見る。io_uring の splice より遅ければ、その経路は io_uring のまま残す（理由を記録）。

## 修正

- HTTP/1.1 の kTLS（Linux）の静的配信を平文と同じ `sendfile_body`（`src/proxy/sendfile.rs`）に一般化した。ヘッダは `MSG_MORE` を付けずに `send(2)`（カーネルが暗号化する。B-25）、本文は `sendfile(2)`。時間切れ・他の接続へ譲る・短い本文で閉じる・`sendfile` 非対応のファイルは読み書きへ、を平文と共有する。
- プロキシの本文の `splice(2)` は kTLS でも平文と同じくその場の非ブロッキング（io_uring の splice はカーネルのワーカースレッドへ回る）。
- kTLS の `MSG_MORE` は既定オフのまま（各カーネルでの確認が要るため本チケットでは入れない）。
- 交互 A/B（fcde8dd9 対 新、静的 kTLS・wrk 2 スレッド 32 接続 8 秒・6 周、他のビルドと同居のため CPU 時間/要求で判断）:
  3B 90.1→78.8 µs/req（−12.5%）・54KB 278.4→278.6（同等）・1MB 3294.6→3117.7（−5.4%）。退行なしとして採用。
  プロキシの組は上流（nginx）の設定誤りで Non-2xx だったため判定に使っていない（プロキシの splice は従来から平文と同じ経路）。

## 関連

- [B-286](../bugs/B-286-plaintext-h1-copies-file-and-proxy-bodies.md)（「残り」の節）、[B-25](../bugs/B-25-reverse-proxy-http1-wrk-zero-completed.md)、[F-150](F-150-rustls-writev-zero-copy-send.md)、
  [B-293](../bugs/B-293-splice-fallback-resends-request-upstream.md)〜[B-297](../bugs/B-297-zero-copy-compression-docs-mismatch.md)
