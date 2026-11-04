# B-277: HTTP/2・HTTP/3 のクライアントがアップロードの途中でストリームをリセットすると、上流には完結した本文として届く

**状態: 修正済み（v0.10.1）**

## 出典

[B-274](B-274-route-client-timeouts-h1-only.md) の統合試験（本文の途中で止めた要求）を書いていて見つけた（実機で確認）。

## 事象

HTTP/2・HTTP/3 のストリーミングの要求本文の中継（chunked で上流へ送る経路）は、クライアントが `RST_STREAM`・`RESET_STREAM`
（または B-274 の打ち切り）で本文を途中でやめると、チャネルの終わりを本文の終わりと区別せずに chunked の終端（`0\r\n\r\n`）を
上流へ送っていた。上流は途中までの本文を完結した要求として処理する（POST の重複・切り詰めたデータの保存）。

## 修正（v0.10.1）

- 要求本文のチャネル（`stream_channel`）が送り手の取り消しで終わったか（`is_cut_off`）を見る。取り消しなら、HTTP/1.1 の上流へは
  chunked の終端を送らずに接続を閉じ（プールへ戻さない）、h2 の上流（h2c・TLS の h2）へは送信端を drop せず（drop すると END_STREAM）
  上流のストリームごと捨てて `RST_STREAM(CANCEL)` にする（`src/proxy/h2.rs`・`src/http3_stream.rs`）。
- 統合試験 `b277_client_reset_mid_upload_is_not_completed_upstream`（h2c・HTTP/2・HTTP/3 で上流が完結した本文を受けないこと）。

## 関連

- [B-274](B-274-route-client-timeouts-h1-only.md)、[B-86](B-86-http3-tls-backend-plaintext-overflow.md)
