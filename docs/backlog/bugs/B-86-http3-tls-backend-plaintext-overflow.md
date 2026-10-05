# B-86: HTTP/3 の TLS バックエンド経路で応答本文が途中で切れ、しかも 200 のまま正常終了する

## 事象

macOS（Apple Silicon、reactor/kqueue）実機の E2E で
`test_http3_request_body_streaming_tls_backend` が失敗した。1,200,000 バイトの
アップロード折り返しに対し、**ステータス 200 のまま本文が 940,825 バイトで終わる**。
単独実行でも約 1/6 の頻度で再現した（547,595 バイトで切れた回もある）。

2026-09-04 の検証で NetBSD x86_64 が同じテストを 2 回とも落としており、
「VM 不調のため未確認」と記録されていたものと同一の不具合である。

## 原因（2 段）

### 1. `TlsBackend::read_into` が rustls の受信平文上限を超えさせる

rustls は受信済み平文が 16KB（`DEFAULT_RECEIVED_PLAINTEXT_LIMIT`）を超えると、
次の `read_tls` を `"received plaintext buffer full"` で拒否する。さらに deframer は
**`read_tls` 1 回あたり最大 4KB** しか取り込まない（`msgs/deframer/buffers.rs` の
`READ_SIZE`）。

`src/http3_stream.rs` の `TlsBackend::read_into` は、生ソケットから最大 16KB の
暗号文を読み、それを `read_tls` + `process_new_packets` の繰り返しで投入するが、
**平文の排出はループの外（次の周回の先頭）でしか行っていなかった**。
「16KB の最大長レコードの末尾 + 後続の小さいレコード群」が 1 回の生 read に
入ると、1 回目の `read_tls` で 16KB レコードが完成し、続く小レコードの平文が
積み増されて上限を超え、次の `read_tls` がエラーになる。

`simple_tls.rs` / `ktls_rustls.rs` の同等ループは既に**レコード投入ごとに排出**
しており、HTTP/3 の全二重ラッパー（F-44）だけが漏れていた。
Linux の既定ビルドは kTLS へ移行して `session = None`（生ソケット）で読むため、
この経路をほとんど通らない。ユーザー空間 rustls しか無い macOS / NetBSD /
OpenBSD で顕在化しやすい。

### 2. 本文途中のエラーを「正常終了（fin）」で閉じていた

`stream_body_length` / `stream_body_chunked` / `stream_body_eof` /
`stream_response_compressed` は、ヘッダ送出後の read エラー・早すぎる EOF・
デッドライン超過を `break` / `Ok(())` で扱い、QUIC ストリームを **fin で正常に
閉じていた**。クライアントからは「200 + 短い本文」に見え、**切り詰めを成功と
誤認する**（Content-Length との不一致を検査しないクライアントでは無言のデータ欠損）。
メインループ側には既に `RespMsg::Error` を「head 送出後ならストリームを
リセットする」と扱う経路があったが、使われていなかった。

## 修正

1. `TlsBackend::read_into` で `process_new_packets` のたびに平文を `drained` へ
   排出する（他の rustls 経路と同じ形）。
2. ヘッダ送出後の本文途中のエラーは `Err(502)`（デッドライン超過は `Err(504)`）を
   返し、`RespMsg::Error` 経由でストリームをリセットする。Content-Length /
   chunked で終端前に EOF になった場合も同様。EOF 終端（`Connection: close`）の
   正常な EOF は従来どおり fin。圧縮経路は欠けた本文を圧縮して返さず 502 を返す。

## 検証

- 決定的な回帰テスト `http3_stream::tests::tls_backend_read_survives_large_record_followed_by_small_records`
  を追加（rustls client/server をメモリ上でハンドシェイクし、16KB レコード 1 本 +
  200 バイトのレコード 80 本を socketpair 越しに一括で流し込む）。**修正を外すと
  `received plaintext buffer full` で失敗する**ことを確認済み。
- macOS 実機で `test_http3_request_body_streaming_tls_backend*` を 20 回連続実行し
  失敗 0（修正前は単独で約 1/6）。
