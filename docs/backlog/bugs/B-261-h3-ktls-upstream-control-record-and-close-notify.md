# B-261: HTTP/3 の上流 TLS 接続で kTLS の制御レコードが `EIO` のまま 502 になり、HTTP/2・HTTP/3・圧縮の経路が close_notify の無い EOF を応答の完了とみなす

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の F-282（KT-01）の検証で `./tests/e2e_setup.sh test ktls` を全件回したところ、HTTP/3 で受けて HTTPS 上流へ流す要求が 502 になった
（プロキシのログに `[HTTP/3] streaming backend read error: Input/output error (os error 5)` が 165 回）。B-259 は HTTP/1.1・HTTP/2 の上流の読み取り
（`KtlsClientStream::read`）だけを直しており、HTTP/3 の上流は別の読み取り経路を持つ。kTLS の構成の E2E は B-259 の 25 件しか見ておらず、
`ktls` の構成は `default` の経路（バッファリング・セキュリティ・Wasm）を生成しないので、全件の結果は経路の欠落の 404 に埋もれていた。

## 事象

- HTTP/3 の上流の読み書き（`http3_stream::TlsBackend`）は、ハンドシェイク済みの TLS 接続を `into_parts()` で分解して持ち、kTLS へ移行した接続は
  生ソケットの `read` で読む。ここに B-259 の制御レコードの処理が無く、TLS 1.3 の上流が送る NewSessionTicket で最初の読み取りが `EIO` になる。
- 同じ `TlsBackend` は、上流が閉じて終わる応答（Content-Length も chunked も無い）を、close_notify の有無を見ずに完了とみなしていた
  （F-282 の KT-01 は HTTP/1.1 のストリーミングとバッファ経路だけを直していた）。切れた応答がストリーミングでは fin、バッファ経路・圧縮では完全な応答として返る。
- 本文を集めてから送る経路（`proxy::transfer::collect_response_body_raw`。HTTP/2 で受けた要求の HTTP/1.1 上流への中継と、HTTP/1.1 の圧縮）も
  close_notify を見ずに EOF を終端にしていた（読み取りの誤りも終端にしていた）。HTTP/2 のクライアントには切れた本文が 200 で届く。

## 影響

- P2（`[ktls] enabled = true` のとき、HTTP/3 で受けて TLS 1.3 の HTTPS 上流へ流す要求）: すべて 502。
- P3（HTTP/2・HTTP/3 で受けるか圧縮して TLS の上流へ流し、上流が閉じて本文を終える応答）: 途中で切れた応答を完全な応答として返す。

## 修正（v0.10.0）

- `TlsBackend::read_ktls`: kTLS へ移行した接続の読み取りが `EIO` を返したときだけ `ktls::consume_control_record` を呼ぶ（B-259 と同じ。平文の読み取りに
  syscall を足さない）。
- `TlsBackend` に close_notify の受信を記録し（rustls の `peer_has_closed`・kTLS の close_notify）、`BackendIo::eof_was_clean` で返す。EOF で終わる応答は、
  TLS で close_notify を受けていなければストリーミングではストリームリセット、バッファ経路・圧縮では 502 にする。
- `collect_response_body_raw`: 接続の終わりで区切る本文は `eof_was_clean()` のときだけ完了とし、読み取りの誤りは失敗にする（HTTP/2 の中継・圧縮は 502）。
- `tests/e2e_setup.sh` の `ktls` の構成は `default` と同じ経路を生成する（全件の E2E を kTLS で意味のあるものにする）。
- 試験用の TLS echo 上流に `x-eof-body: clean|abrupt`（長さの無い応答を close_notify 付き・無しで閉じる）と、それへのバッファ経路のルート
  `/echo-eof-tls-full/*` を足した。
- 試験: 単体 `tls_backend_eof_reports_close_notify`・`collected_until_close_body_needs_clean_eof`。E2E `test_f282_*_tls_upstream_eof_body_*`（HTTP/1.1・
  HTTP/2・HTTP/3、既定と kTLS の構成）。`./tests/e2e_setup.sh test ktls` の全件（修正前は 84 件失敗、後は 577 件すべて通過）。

## 関連

- [B-259](B-259-ktls-rx-control-record-eio.md)、[F-282](../features/F-282-security-review-followup-hardening.md)（KT-01）
