# B-444: メッシュの HTTP/1.1 の 503 等のローカル応答が、本文を読み捨てられなくても接続を続け、残りの本文を次の要求として読んでいた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Mesh HTTP/1.1 503 replies keep the connection open after a failed body discard, so leftover body bytes are parsed
as new requests (CL.0-style desync)」から確認。

## 事象

- `mesh::http1::forward`・`forward_to_h2` の本文を送る前に返す 503（健康な上流が無い・サーキットブレーカー・上流への
  接続の失敗・HTTP/2 の多重化接続が使えない）は、`discard_body` の結果を見ずにキープアライブを続けていた。読み捨ては
  残りが 1MiB を超える・chunked が壊れている・30 秒止まると途中でやめるので、残りの本文を同じ接続の次の要求として
  読んだ。`mux.open()` が `None` の経路は本文を読み捨てずに返していた（`Decision::Local` の経路は正しく閉じていた）。
- 接続を使い回す前段（ゲートウェイの前の L7 ロードバランサ・他のサイドカーの上流のプール）の後ろで、本文に隠した要求が
  前段の検査を通らずに届き、応答の列もずれた。

## 対応

- 本文を読み終える前に返すローカル応答は `reply_before_body` にまとめ、本文を読み捨ててから返し、読み捨てられなければ
  `Connection: close` を付けて閉じる（`Decision::Local` と同じ）。`mux.open()` が `None` の経路も読み捨てる。閉じるときも
  読み捨ては試みる（未読のデータを残して閉じると RST になり、相手は応答を読めない）。

## テスト

- `mesh::http1::tests::early_reply_closes_unless_the_body_is_discarded`（1MiB を超える本文・壊れた chunked は閉じ、
  読み捨てられた本文は続ける）。
