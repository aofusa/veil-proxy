# B-402: HTTP/3 のワーカーが UDP のソケットを 1 本ずつ bind していたので、起動の直後に始まった QUIC の接続が止まることがあった

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

## 事象

手元の全試験（自己完結型 E2E の epoll 版）で `b351_http3_with_several_workers_starts_cleanly`（`threads = 3`・HTTP/3 有効）が
1 回だけ HTTP/3 の応答待ちの `ConnectionError(Timeout)` で落ちた。単独で 10 回・同時に 12 本回しても再現しない。試験は TCP の
ポートが開いたら HTTP/3 の要求を送る。

## 原因

`entry.rs` は TCP のリスナーを先に作り、HTTP/3 の各ワーカーはスレッドの中で quiche の設定（証明書の読み込み等）を作ってから
自分の UDP ソケットを bind していた。TCP のポートが開いた時点で UDP のソケットがまだ一部しか bind されていないことがある。

UDP の `SO_REUSEPORT` は、データグラムが届くたびにグループのソケットの数でフローの振り分け先を決める（TCP は接続の確立後は振り分け
直さない）。最初の何本かが bind した時点で受けた QUIC の接続は、残りのワーカーが bind するとソケットの数が変わって以降のパケットが
別のワーカーへ渡り、その接続を知らないワーカーに捨てられる。クライアントは応答を待ち続けて時間切れになる。本番でも、起動の直後
（数ミリ秒）に始まった HTTP/3 の接続は同じ理由で止まり得た。

## 修正

- `listener_workers::spawn_http3_listener` が、ワーカーを起こす前にメインスレッドで全ワーカー分のソケットを続けて bind し、
  ワーカーへ渡す（`run_http3_server(addr, socket, config)`。中の bind を外した）。グループの大きさが変わるのは bind のループの間
  だけになる。それより前に届いたパケットはポートが閉じているので相手が送り直し、後に届いたパケットは最後まで同じワーカーへ行く。
- bind できなかった分は従来と同じ `Server error` を出してワーカーを起こさず、起こした本数を返す（B-351 の「bind できたワーカー
  だけを証明書リロードの待ち人数に数える」はそのまま）。
- ワーカーが証明書の読み込み等で起動に失敗すると、渡されたソケットはその場で閉じる（以前は bind する前に終わっていた。B-50 の
  ように UDP のポートが閉じたままになるのは同じ）。

## 確認

- clippy（`full`・`full,epoll`、`-D warnings`）、`--no-default-features`・既定の feature のビルド。
- 自己完結型 E2E（io_uring・epoll）を 2 回ずつ回して 55 件がすべて通り、`b351_http3_with_several_workers_starts_cleanly` を
  バックエンドごとに 10 回続けて回して全部通った。
- E2E（HTTP/3 を含む）: epoll は 594 件が通った。io_uring で落ちた 1 件（`test_http3_wasm_local_response`）は
  [B-110](B-110-netbsd-x86_64-wasm-waf-load-flaky.md)（Proxy-Wasm の起動時のコールバックの実時間の締切。この変更とは別の
  不具合で、同じブランチで直した）。
- macOS（llm、`full-macos`）で clippy・単体・自己完結型 E2E の `b351`・`http3`、Windows（aofish、`full-windows`）で clippy・
  単体・自己完結型 E2E（50 件）が通った。
