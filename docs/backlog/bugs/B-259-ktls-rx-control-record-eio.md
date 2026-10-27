# B-259: kTLS の受信で TLS 1.3 の NewSessionTicket・close_notify が `EIO` になり、上流への kTLS 接続が最初の応答で 502 になる

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の B-258 の検証で `./tests/e2e_setup.sh test ktls` を回したところ、kTLS の構成ではプロキシ経由の HTTPS 上流への要求が全て 502 になった
（`Backend closed connection (kTLS EIO or connection reset): Input/output error`）。B-258 の変更を外しても同じで、それ以前からの不具合。
CI は kTLS の構成の E2E を回していない。

## 事象

- Linux の kTLS は、平文以外のレコード（アラート・ハンドシェイク）を制御メッセージ無しの `read(2)`（io_uring の READ も同じ）に渡さず `EIO` を返す。
  レコードは受信キューに残り、`recvmsg(2)` に `TLS_GET_RECORD_TYPE` の制御メッセージの場所を渡すと取り出せる。
- veil は kTLS の受信（`KtlsClientStream`・`KtlsServerStream` の `read`）で `EIO` をそのまま返す。
- TLS 1.3 のサーバはハンドシェイクの後に NewSessionTicket を送る（veil 自身も `[tls] session_resumption = "tickets"` が既定。F-183）。
  そのため `[ktls] enabled = true` のとき、上流への HTTPS 接続は応答の前に `EIO` になり、`https.rs` は「上流が閉じた」とみなして 502 を返す。
- クライアント側の close_notify も `EIO` として届き、閉じたことと区別できない。

## 影響

- P2（`ktls = true` のときだけ、TLS 1.3 の HTTPS 上流）: 上流が TLS 1.3 でセッションチケットを送るなら（ほぼ全て）kTLS の上流接続が使えない。

## 改修案

- `read` が `EIO` を返したときだけ `recvmsg(MSG_PEEK)` で制御レコードの種類を見て、NewSessionTicket は読み捨てて読み直し、close_notify は EOF、
  その他のアラートは誤りにする。平文の読み取りには syscall を足さない。

## 修正（v0.10.0）

- `src/ktls.rs` に `consume_control_record` を足し、kTLS の受信（`KtlsServerStream`・`KtlsClientStream` の `read`）が `EIO` を返したときだけ呼ぶ: `recvmsg(MSG_PEEK | MSG_DONTWAIT)` と `TLS_GET_RECORD_TYPE` の制御メッセージで待っているレコードの種類を見て、ハンドシェイク（NewSessionTicket）は読み捨てて読み直し、close_notify は EOF（`Ok(0)`）、その他のアラートは `ConnectionAborted`、それ以外（KeyUpdate・平文のレコードでの本当の `EIO`）は元の誤りを返す。平文の読み取りには syscall を足さない（Linux のみ。`recvmsg` は seccomp の許可リストにある）。
- 試験: `./tests/e2e_setup.sh test ktls` の kTLS の E2E 25 件（修正前は上流への要求が全て 502 で 13 件失敗）が通過。kTLS の構成でプロキシ経由の HTTP/1.1・HTTP/2 の要求が 200。
- kTLS の構成の E2E は CI では回していない（ランナーの tls モジュールの有無に依存）。kTLS の経路を変えたら手で回す。

## 関連

- [B-258](B-258-ktls-handover-partial-record-left-in-rustls.md)、[F-282](../features/F-282-security-review-followup-hardening.md)（KT-01 の制御レコード）、
  [F-183](../features/F-183-veil-allocator-and-library-tuning.md)
