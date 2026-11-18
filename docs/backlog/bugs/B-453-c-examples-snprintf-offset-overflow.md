# B-453: C の例（c-kit の `/headers`・c-features の multipart の一覧）が snprintf の返り値をそのまま足し、長い入力でスタックの buf を越えて書いた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Long x-multi request header makes `sizeof buf - off` underflow, and snprintf writes past the 512-byte stack
buffer」から確認。

## 事象

- `examples/api/c-kit/kit.c` の `headers` は `x-multi` の値ごとに snprintf の返り値（切り詰める前の長さ）を `off` に
  足していた。ループの条件は次に足す前にしか見ないので、長い値で `off` が 512 を越え、最後の snprintf は
  `buf + off` へ、折り返した `sizeof buf - off` を上限として書き、スタックを壊した（`_FORTIFY_SOURCE` なら abort）。
- 同じ形が `examples/api/c-features/features.c` の `list_parts`（multipart のパートの名前・ファイル名はクライアントが
  決める）にもあり、ループの後の `out[off] = 0` が範囲外に書いた。利用者は例を写して使う。

## 対応

- snprintf の返り値が負なら止め、残りの大きさ以上なら `off` を末尾（`sizeof buf - 1`）で止める（切り詰めて返す）。

## テスト

- `tests/api/library_e2e.sh` の「C header_next long value is truncated」・「C multipart long names are truncated」と、
  その後もサーバが答えること。
