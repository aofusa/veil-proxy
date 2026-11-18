# B-468: C の L4・アップグレードの接続で、on_connect が書いたデータの送信に失敗すると on_close を呼ばずにセッションを捨てていた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「C L4/upgrade session is dropped without on_close when the first write fails, leaking per-connection C state」
から確認。

## 事象

- `App::l4_tcp_blocking`・`Upgraded::serve_blocking` は、`connect`（C の `on_connect`）が返したデータの送信に
  失敗すると（相手が RST した等）`?`・`return` で抜け、`on_close` を呼ばずにセッションを捨てていた。停止で接続の
  タスクが途中で捨てられたときも呼ばなかった。C の API は `on_close` で `veil_l4_set_conn_data` の状態を解放する
  ように案内しているので、接続して直ちに切る相手が接続ごとに C の状態を漏らせた。

## 対応

- セッションができた後は、どの出口でも `on_close` をハンドラ用プールで 1 回だけ呼ぶ（`api::l4::SessionCloser`。
  ふつうは待って呼び、途中で捨てられたら待たずにプールへ渡す。`on_data` の panic で毒の付いた状態でも呼ぶ）。
- C の `CSession` に `on_close` を呼んだ印と、呼ばれないまま捨てられたときの Drop を足した（`CWsSession` と同じ
  最後の守り）。

## テスト

- `api::l4::tests::session_closer_calls_on_close_once_on_every_exit`（待って閉じても、閉じる前に捨てても 1 回）。
