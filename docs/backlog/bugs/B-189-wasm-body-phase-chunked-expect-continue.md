# B-189: chunked + `Expect: 100-continue` の要求に、Proxy-Wasm の要求本文の段が 100 Continue を返さずに本文を待つ

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 のデータプレーンのレビュー（`docs/artifacts/v0100/review-dataplane.md` の L-2）。F-271。

## 現象

`allow_request_body_read` のモジュールが付いたルートへ、`Transfer-Encoding: chunked` と `Expect: 100-continue` の要求が来ると、
veil は 100 Continue を送らないまま本文を読みに行き（上流にもまだつないでいないので上流の 100 Continue も来ない）、律儀に
待つクライアントと互いに待って `READ_TIMEOUT` の後に 400 になる（curl は 1 秒待って送るので遅くなるだけ）。要求ヘッダの段の
ローカル応答の後の本文の読み捨てでも同じく待っていた。

## 原因

100 Continue は Content-Length の要求にだけ返していた（chunked は上流の 100 Continue に任せている）。F-271 の本文の段は
上流へつなぐ前に本文を全部読む。

## 直したこと

- 本文の段に入るとき、chunked で `Expect: 100-continue` があれば 100 Continue を返してから読む。上流へは本文を Content-Length で
  揃えて送るので `Expect` を外す（上流の 100 Continue がクライアントへ重ねて届かないように）。
- 要求ヘッダの段のローカル応答で、chunked + `Expect: 100-continue` の本文がまだ 1 バイトも届いていなければ、読み捨てを待たずに
  応答の後で接続を閉じる。
- 文書: `docs/guide/wasm.md`（英日）。

## 試験

- E2E `b189_wasm_body_phase_sends_100_continue_for_chunked`（`tests/standalone_e2e.rs`。ヘッダだけ送って 100 Continue を待ち、
  届いてから本文を送って応答を確かめる。直す前は 100 Continue が来ずに時間切れ）。
