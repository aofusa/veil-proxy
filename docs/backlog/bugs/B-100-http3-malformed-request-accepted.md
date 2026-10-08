# B-100: HTTP/3 で malformed なリクエスト（疑似ヘッダ違反）に 200 を返していた

## 事象

`tools/container_security` の h3spec が、実際には毎回 2 ケース目の後で打ち切られていて
（1 ケースの待ちに全体上限と同じ 60 秒を渡していたため。同じ変更でハーネスを修正）、
全 49 ケースを走らせたのは今回が初めてだった。結果は 17 件失敗。

このうち veil 自身の不具合は HTTP/3 層の 4 件で、すべて同じ原因だった:

- 疑似ヘッダの重複（`:method` が 2 つ）
- 必須疑似ヘッダの欠落（`https` なのに `:authority` も `Host` も無い等）
- 禁止された疑似ヘッダ（`:status` をリクエストに付ける等）
- 通常フィールドの後ろに疑似ヘッダ

いずれも RFC 9114 §4.1.2 / §4.3.1 で malformed（H3_MESSAGE_ERROR）とすべきものだが、
veil は 200 を返していた。

## 原因

quiche の h3 層はフィールドセクションの妥当性を検査しない。veil の `classify` は
`:method` が無ければ `GET`、`:path` が無ければ `/` と見なしており、検査する場所が
どこにも無かった。

あわせて、**既に受け付けたストリームの 2 つ目の HEADERS（リクエスト trailers）を新規
リクエストとして再分類していた**（`Event::Headers` をストリームの状態を見ずに一律
`classify` へ渡していた）。

## 修正

- `h3_check_field_section`（確保なし・1 パス）を追加し、`Event::Headers` を受けた時点で
  検査する。malformed なら `H3_MESSAGE_ERROR` で接続を閉じる（§8: ストリームエラーを
  コネクションエラーとして扱ってよい）。検査項目は疑似ヘッダの重複・未知/禁止
  （拡張 CONNECT を広告していないので `:protocol` も禁止）・通常フィールドの後ろの疑似ヘッダ・
  大文字のフィールド名・接続固有フィールド（`connection` 等、`te` は `trailers` のみ可）・
  必須疑似ヘッダ（CONNECT は `:authority` のみ、それ以外は `:method`/`:scheme`/`:path`、
  `http(s)` は `:authority` か `Host`）。
- 既知のストリームの疑似ヘッダ無しセクションは trailers として扱い、新規リクエストに
  しない（転送はしない＝従来どおり）。
- vendoring 版 quiche（`third_party/quiche`）で、認証済みパケットの予約ビットが 0 でなければ
  PROTOCOL_VIOLATION で閉じる（RFC 9000 §17.2 の MUST。upstream は未検査）。

h3spec: 32/49 → 38/49。

## 残り 11 件（quiche の設計。対応しない）

| ケース | 理由 |
|---|---|
| TRANSPORT_PARAMETER_ERROR × 8 | ClientHello（最初の Initial）の処理中に失敗するため、quiche は `recv_count == 0` の接続を CONNECTION_CLOSE を送らずに即座に閉じる（`Connection::close`）。未認証の 1 パケット目に応答しないのは反射・増幅を避けるための意図的な挙動で、接続は確立しない（＝受理はしていない） |
| CRYPTO in 0-RTT | veil は 0-RTT（early data）を有効にしていないため、0-RTT パケットは復号できずに捨てられる。quiche 自体は 0-RTT 中の CRYPTO フレームを拒否する（`Frame::allowed_in_pkt`） |
| QPACK_ENCODER/DECODER_STREAM_ERROR | quiche の QPACK は動的テーブルを使わず（容量 0）、エンコーダ/デコーダストリームの命令を解釈しない |

いずれも「不正な入力を受理してしまう」ものではなく、エラーコードを返さずに捨てる・閉じる
ものである。

## テスト

- 単体: `http3_server::tests::test_h3_field_section_*`（正常なリクエスト・CONNECT・trailers、
  および上記の malformed 各種）
- h3spec（`tools/container_security`、`H3SPEC_REQUIRED=1`）
