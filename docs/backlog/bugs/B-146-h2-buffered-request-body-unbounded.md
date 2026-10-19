# B-146: HTTP/2 のバッファ経路の要求本文に上限が無く、フロー制御のクレジットを返し続けていた

**状態: 完了（feat/v080-release、2026-10-18。F-236 の再走査の F3）**

## 症状

ストリーミングの対象でないストリーム（静的配信・Respond・JWT・リダイレクト・ルートの無い要求・Wasm・`buffering = full`・
HTTP/1.1 の上流への gRPC）は、END_STREAM まで DATA を `request_body` へ溜め、受け取るたびに接続とストリームの受信ウィンドウを
補充していた。`max_request_body_size` は本文を溜め終わってから見るので、終わらない本文を多くのストリーム・接続で送ると
プロセスのメモリを使い切れた。

## 修正

`Stream::recv_data` で、溜める前に上限（`l7::max_buffered_request_body`。全ルートの `max_request_body_size` の最大、最低 10MB。
設定のロード・リロードで決める）を見て、超えたらストリームを ENHANCE_YOUR_CALM で断る。

## 関連

F-236、B-147。
