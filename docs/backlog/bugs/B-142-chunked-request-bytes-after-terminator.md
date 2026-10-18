# B-142: chunked の要求で、終端より後ろのバイトを上流へ送っていた（要求の密輸）

**状態: 完了（feat/v080-release、2026-10-18。F-236 の再走査の F1）**

## 症状

HTTP/1.1 の chunked の要求は、本文の長さが決まっていないので B-129 のパイプラインの切り離しをしておらず、ヘッダの後ろに
読み取り済みのバイトを全部「本文の先頭」として上流へ書いていた。`transfer_chunked_body` も、デコーダが終端を見つけた読み取りの
バッファを丸ごと書いていた。

`POST /public` に `Transfer-Encoding: chunked` と `0\r\n\r\n` を付け、直後に `GET /admin ...` を同じ書き込みで送ると、veil は
`/public` だけを照合・検査し、上流は `/admin` も処理した。`/admin` のルートの IP 制限・JWT・Wasm は通らず、余った応答はプールの
接続に残ってほかの利用者へ返り得た。

## 修正

- `h1.rs`: chunked の要求は、読み取り済みの範囲で厳格なデコーダ（B-143）が終端を見つけたら、その直後で切り、残りを次の要求にする。
- `transfer_chunked_body`: `feed_counted` で終端の位置を知り、終端より後ろは上流へ書かない（同じ読み取りに入った次の要求は捨てる）。

## 試験

`http_utils::tests::strict_request_decoder_rejects_lenient_framing_and_reports_end`、E2E。

## 関連

F-236、B-143、B-129、B-135。
