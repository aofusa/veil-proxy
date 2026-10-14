# B-128: io_uring の制限付きリングが IOSQE_BUFFER_SELECT を拒否し、HTTP/3 の multishot 受信（F-130 C2）が本番で一度も動いていなかった

**状態: 完了（feat/api-roadmap-impl、2026-10-14。F-205 の設計確認で発見）**

## 症状

ワーカーの io_uring は `IORING_REGISTER_RESTRICTIONS` で使う SQE オペコードだけを許可している（`PROXY_ALLOWED_OPCODES`、
`ring.rs::apply_restrictions`）。カーネルは制限を登録したリングでは **SQE フラグも** 検査し、
`IORING_RESTRICTION_SQE_FLAGS_ALLOWED` で許可していないフラグ付きの SQE を `-EACCES` で失敗させる
（`io_check_restriction`）。veil はフラグの許可を登録していなかったため、provided buffer を選ぶ
`IOSQE_BUFFER_SELECT` 付きの op は本番のリングで全部 `-EACCES` になっていた。

これを使うのは HTTP/3 の UDP 受信の F-130 C2（`MultishotUdpRecv`、multishot `RECVMSG` + provided buffer ring）だけで、
初回の失敗で C1（パイプライン化した `RECVMSG`）へ戻る作りだったため、HTTP/3 は動き続け、誰も気づかなかった。
単体試験は制限の無いリングで動くので C2 を試しており、E2E・性能計測は本番と同じ制限付きのリング（= C1）を測っていた。

## 確認

`ring.rs` の単体試験 `restrictions_allow_only_buffer_select_flag`: 制限付きのリング（NOP・RECV を許可）で、
`IOSQE_BUFFER_SELECT` 付きの RECV（登録していないバッファ群）が修正前は `-EACCES`（-13）、修正後は制限を通って
`-ENOBUFS` になる。`IOSQE_ASYNC`・`IOSQE_IO_LINK` は修正後も `-EACCES`。

## 修正

`apply_restrictions` で `IORING_RESTRICTION_SQE_FLAGS_ALLOWED` に `IOSQE_BUFFER_SELECT` だけを登録する。新しいオペコードは
増えない。許可するフラグは buffer select の 1 つだけで、リンク・非同期実行・固定ファイル等は今どおり拒否する。

## 影響と確認

- HTTP/3 の受信が本番で初めて C2（multishot）になる。HTTP/3 の E2E（`tests/e2e_setup.sh`、io_uring）と HTTP/3 の
  交互 A/B（`tools/perf`）で確かめる（結果は F-205 のチケットに記録）。
- F-205（TCP のキープアライブ受信を provided buffer の RECV 1 回にする）はこの修正が前提。
