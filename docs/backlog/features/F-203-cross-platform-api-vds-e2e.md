# F-203: API ライブラリ・VDS の E2E を各 OS で確認する

**状態: 完了（feat/cross-platform-api-vds-e2e）**

## 要望

API ライブラリと VDS の E2E を FreeBSD・OpenBSD・NetBSD・macOS・Windows でも確認する（Kubernetes は対象外）。
動かないものは直して動くようにする。

## 環境

| OS | 実行場所 | 備考 |
|---|---|---|
| FreeBSD 14.3 x86_64 | ローカルの QEMU/KVM（`tools/qemu/bsd-vm.sh`） | 試験中に `/` が足りなくなったので 28G → 44G に拡張 |
| OpenBSD 7.9 x86_64 | 同上 | `/usr/obj` が 8G しかないのでサンプルのビルドは `/home` に置いた |
| NetBSD 10.1 x86_64 | 同上（メモリ 7GB。他の VM と同時に動かさない） | |
| macOS 15.5 arm64 | `ssh llm`（Apple Silicon、ネイティブ） | |
| Windows 11 x86_64 | `ssh aofish`（ネイティブ、Git Bash + MSVC） | HTTP/2 対応の curl（curl.se の Windows 版）と protoc を `Documents/aofusa/e2e-tools` に置いた |

## 試験

- `tests/standalone_e2e.rs`（OS の `full*` feature）: VDS（REST の配信と NACK・Python 版/Rust 版の制御プレーン・gRPC
  ストリームの h2c/TLS）、平文リスナー（B-122・F-200）、環境変数による上書き（F-179）、証明書の差し替え（F-180、unix）、B-123。
- `tests/api/library_e2e.sh all`: 本体ビルドの非退行・Rust の総合例（ルーティング・検証・OpenAPI・tower・GraphQL・L4・
  ストリーミング・HTTP/1.1 と h2c・SIGHUP・既定値・環境変数）・gRPC の相互運用（tonic の h2c と quinn の h3）・C（動的・静的）・Python。
- `tools/tokio-compat/ecosystem`（hyper・reqwest・tower-http・axum・sqlx）。PostgreSQL は Linux だけ。

## 結果

| OS | standalone_e2e（VDS を含む） | library_e2e（非退行・Rust・gRPC・C・Python） | tokio 互換の実ライブラリ |
|---|---|---|---|
| Linux x86_64 | 17/17（io_uring・epoll）。あわせて lib 1053・integration 60・E2E 565（io_uring・epoll） | すべて ok（gRPC は io_uring と epoll のビルド） | ok（F-201 で確認。PostgreSQL を含む） |
| macOS 15.5 arm64 | 17/17 | すべて ok | ok（PostgreSQL は未実施） |
| FreeBSD 14.3 x86_64 | 17/17 | すべて ok | ok（同上） |
| OpenBSD 7.9 x86_64 | 17/17 | すべて ok | ok（同上） |
| NetBSD 10.1 x86_64 | 17/17（B-126 の修正後） | すべて ok | ok（同上） |
| Windows 11 x86_64 | 16/16（F-180 の Secret の差し替えは unix のみ） | すべて ok（SIGHUP・シグナルの試験は省く。C は MSVC） | 対象外（`net` が無い。F-204） |

試験中の環境要因（コードの問題ではないもの）:

- OpenBSD・FreeBSD の VM でディスクが足りず、リンクが途中で切れた実行ファイルが `ld.so` の SIGFPE で落ちた
  （消して作り直すと通る）。高負荷のホストで Python の制御プレーン・サンプルの起動が遅く、待ち時間を延ばした。

## 直したもの

- B-123: HTTP/2 の要求本文の受け手がメインループを起こさず、1MiB を超える本文で固まった（reactor）。
- B-124: macOS・OpenBSD で veil 本体がビルドできなかった（F-200 の取りこぼし）・macOS で単体テストがビルドできなかった。
- B-125: C・Python 版とサンプル・VDS の Rust 制御プレーンが Linux 以外でビルドできなかった。
- B-126: NetBSD で制御プレーン（VDS・ACME）への接続が拒否されても次のアドレスを試さなかった。試験の接続待ちも同じ。
- 試験ハーネス:
  - `tests/api/library_e2e.sh`: OS の `full*` を選ぶ。`sed -i`・`nproc`・`timeout`・`nc` の差を吸収（L4 のエコーは Python）。
    C の静的リンクのライブラリは rustc の `native-static-libs` から取り、BSD は GNU make、Windows は MSVC（vcvars）でビルド。
    Windows は venv の `Scripts`・`.exe`・`CONDA_PREFIX`・openssl の引数のパス変換を扱い、SIGHUP の試験を省く。
    Linux では gRPC を epoll のビルドでも回す（B-123）。bash 3.2（macOS）で動く書き方にした。
  - `crates/veil-python/tests/e2e.sh`: Windows は venv のランチャの子プロセスごと止め、シグナルの試験を省く。
  - `tests/standalone_e2e.rs`: 設定に埋め込むパスを `/` 区切りにする（Windows の `\` が TOML のエスケープになる）。
    Windows に無い SIGHUP・SIGTERM を unix 限定に。Python の名前は `PYTHON3` で変えられる。
  - `tests/runtime_cancellation_test.rs`: WSAPoll のビルドでコンパイルできなかった。
  - `tools/qemu/bsd-vm.sh sync`: `crates/`・`proto/` を転送していなかった（F-188 以降、ゲストでビルドできなかった）。
  - standalone の TLS クライアントは `veil::tls_provider::provider`（OpenBSD・NetBSD は ring）を使う。
  - `tools/tokio-compat/ecosystem`・`tools/api-bench/axum-veil` の `Cargo.lock` の aws-lc-rs を本体と同じ 1.18.1 に
    揃えた（1.18.2 が引く aws-lc-sys 0.46 は FreeBSD で bindgen が通らない）。
  - 1MiB の本文は `dd` で作る（OpenBSD の `head` に `-c` が無い）。C のサンプルは毎回作り直す（同期で別 OS の実行ファイルが
    紛れ込んだ）。起動待ちは最大 30〜60 秒にし、プロセスが終了したらすぐ失敗にする。

## 残り

- F-204: Windows の `veil::tokio` に `net`・`signal` が無い（axum・hyper・reqwest・sqlx はシムの上でビルドできない。
  `tokio-compat-bridge` と `tokio_service` で載せるサービスは動く）。
