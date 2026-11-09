# B-358: OpenBSD x86_64 で試験用の制御プレーン（`tools/vds/server`）が TLS のハンドシェイクで SIGSEGV になる

**状態: 修正済み（feat/v0120-perf、2026-11-10）**

v0.12.0 の最後の検証で x86_64 の BSD の自己完結型 E2E を回して見つけた。veil 本体（OpenBSD・NetBSD は `ring`）には関係しない。

## 事象

OpenBSD 7.9 x86_64 の `f199_vds_grpc_stream_tls`（gRPC の双方向ストリーム、TLS + ALPN h2）が単独でも 3 回続けて落ちる。参照の制御プレーン
`tools/vds/server` が最初の TLS のハンドシェイクでプロセスごと落ちて、veil が設定を受け取れない。同じ試験は OpenBSD aarch64 では通っていた。
平文（`f199_vds_grpc_stream_h2c`）と REST は x86_64 でも通る。

## 原因

`tools/vds/server` は tonic の `tls-aws-lc`（AWS-LC）を全 OS で使っていた。

- `ktrace` の結果は `veil-vds-server PSIG SIGSEGV code=SEGV_ACCERR addr=0x74cd488fac0 trapno=6`（アクセス権の違反）。
- `nm` で見ると、この番地は `.text` の中の `Lcurve25519_x25519base_alt_edwards25519_0g`（AWS-LC が取り込んでいる s2n-bignum の x86_64 のコードの定数表）。
- OpenBSD の x86_64 は `.text` を実行専用で載せるので、コードの隣の定数表を読むと権限違反になる（`SEGV_ACCERR` と番地からの判断）。aarch64 のコードにはこの置き方が無い。

本体と E2E の試験用のクレートは同じ種類の問題（F-122・B-51）で OpenBSD・NetBSD を `ring` にしているが、後から足した制御プレーン（F-199）は揃っていなかった。

## 対応

`tools/vds/server/Cargo.toml` の tonic の TLS の feature を target で分ける。OpenBSD・NetBSD は `tls-ring`、それ以外は従来の `tls-aws-lc`
（`src/tls_provider.rs`・`tests/test_backends/Cargo.toml` と同じ分け方）。

## テスト

修正の前後で OpenBSD x86_64 の `f199_vds_grpc_stream_tls` を単独で 3 回ずつ回した（前は 3 回とも失敗、後は 3 回とも成功）。
修正の後は OpenBSD x86_64 の自己完結型 E2E が 49 件すべて通った（2 回）。`ring` に揃えた NetBSD x86_64 も `f199` の 3 件（2 回）と自己完結型 E2E の 49 件が通った。aarch64 の OpenBSD・NetBSD も `f199` の 3 件と自己完結型 E2E の 49 件が通った。
