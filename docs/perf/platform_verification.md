# 全プラットフォーム検証（v0.8.3 の全件と v0.8.4 の回し直し）

## v0.8.4（2026-10-22〜23）

v0.8.4 の変更（B-158・B-159。依存・イメージの土台・メトリクスのラベル・試験のハーネス）について、影響を受けるところを回し直した。
コードの変更は `src/metrics.rs` と設定のロードの 2 か所、依存の変更は wasmtime の feature（vendoring 版を含む）。

| 対象 | 環境 | 結果 |
|---|---|---|
| Linux x86_64 単体・統合・standalone | ホスト（`full`） | 1088・60・23（VDS の Rust の参照サーバ（tonic 0.14）の gRPC h2c・TLS を含む） |
| Linux x86_64 E2E | io_uring・epoll | 569・569 |
| clippy `-D warnings` | Linux（`feature-check.sh` の 92 組）・macOS（`full-macos`）・Windows（`full-windows`） | 指摘 0 |
| macOS aarch64 単体 | 実機（`full-macos`） | 1004 |
| Windows x86_64 単体 | 実機（`full-windows`） | 957 |
| NetBSD x86_64 単体・統合 | QEMU + KVM（`full-netbsd`、vendoring 版 wasmtime） | 1002・60 |
| container_security | `veil:new084`（glibc・`full`。土台は Wolfi） | 26 成功・失敗 0（判定を厳しくした後。B-159） |
| イメージの起動 | `full-container` の glibc（`image-smoke.sh` の `container`・`defaults`） | すべて ok |
| grype（High 以上で失敗、直す版の無いものを含む） | `full-container` の glibc イメージ・glibc の土台（amd64・arm64） | 0 件 |
| cargo-audit（全 Cargo.lock、警告も失敗）・cargo-deny | — | 0 件・ok |
| crates.io の公開の dry-run | `publish-crates.sh --dry-run` | 4 クレートとも検証のビルドまで ok |

---

## v0.8.3

v0.8.3 の公開（タグ `v0.8.3` = `ff15ea56`）の後、対象のすべての OS・アーキテクチャで、単体・統合・standalone・E2E・ライブラリの試験を回した結果
（2026-10-21〜22）。

- Linux x86_64 以外は、実機（macOS・Windows）か実カーネルの VM（`tools/qemu/`）の上で、ネイティブにビルドしたものを試験している。
- 試験の後に入れた変更は、`main` に入れる前に Linux で単体・E2E（両バックエンド）を回し直した。変更は 2 つ。
  - HTTP/2 の最後の DATA フレームに END_STREAM を付ける（`403f79bf`）
  - 試験とハーネスの修正

| OS | アーキ | 環境 | feature | 単体 | 統合 | standalone | E2E | ライブラリ |
|---|---|---|---|---|---|---|---|---|
| Linux | x86_64 | ホスト（io_uring） | `full` | 1085 | 60 | 23 | 569 | 234 |
| Linux | x86_64 | ホスト（epoll reactor） | `full,epoll` | — | — | — | 569 | — |
| Linux | aarch64 | QEMU + HVF（Ubuntu、io_uring） | `full` | 1083 | 60 | 23 | 569 | 234 |
| macOS | aarch64 | 実機（kqueue reactor） | `full-macos` | 1001 | 60 | 23 | 569 | 200 |
| Windows | x86_64 | 実機（WSAPoll reactor） | `full-windows` | 954 | 54 | 22 | スモーク※1 | 199 |
| FreeBSD | aarch64 | QEMU + HVF（14.3） | `full-freebsd` | 1022 | 60 | 23 | 569 | 173 |
| FreeBSD | x86_64 | QEMU + KVM（14.3） | `full-freebsd` | 1022 | 60 | 23 | 569 | 172 + C 47※2 |
| OpenBSD | aarch64 | QEMU + HVF（7.9） | `full-openbsd` | 999 | 60 | 22 | 568（無視 1） | 173 |
| OpenBSD | x86_64 | QEMU + KVM（7.9） | `full-openbsd` | 999 | 60 | 22 | 568（無視 1） | 173 |
| NetBSD | aarch64 | QEMU + HVF（10.1） | `full-netbsd` | 999 | 60 | 22 | 569 | 173 |
| NetBSD | x86_64 | QEMU + KVM（10.1） | `full-netbsd` | 999※3 | 60 | 22 | 569 | 175※3 |

すべて失敗 0（下の注のとおり、試験の側を直して回し直したものを含む）。

- 件数の違いは、feature とプラットフォームの `cfg` による。
  - Windows: Unix 専用の試験が対象外。
  - BSD: `full-*` に含まれない機能の試験が対象外。
- ライブラリの件数は `tests/api/library_e2e.sh all` の確認の数（Rust・GraphQL・gRPC・C・Python）。
  - BSD は grpcio が入らないので、その 1 件を飛ばす。
  - 数え方は v0.8.3 で 4 件増えた（アクセスログの確認。F-244）。
- BSD の 3 OS は、両アーキで OS 固有のサンドボックス（FreeBSD の capsicum・OpenBSD の pledge/unveil・NetBSD の chroot + 特権降格）の下での
  E2E（`bsd-vm.sh <os> <arch> security-e2e`）も通った。

※1 Windows は bash の E2E のハーネスが UDS のバックエンドなど Windows で使えない機能を含むので、`veil.exe` を前段（静的配信 + `/proxy/`
の中継）と後段の 2 つ起動して確かめた: 静的配信 200・h2c 200（HTTP/2）・中継 200・後段を止めた後 502・アクセスログ（JSON）が標準出力に出る。

※2 FreeBSD x86_64 のライブラリは、C の `hello-static` のアクセスログの確認だけが、grep の「Binary file matches」で 1 件落ちた。確認を `grep -a` にし、
ログに制御文字が無いことの確認を足して C を回し直し、47 件すべて通った（ログは制御文字を含まない）。

※3 NetBSD x86_64 は 2 件。
- 単体の `measure_pooled_vs_fresh_connection` が並列の試験の負荷で 1 回落ちた（模擬サーバへの 5 秒の期限で EAGAIN）。単独では 3 回とも通る。
  期限を 30 秒にした。
- ライブラリの 1 回目は、VM が GraphQL の例のビルドの途中で応答しなくなった（ゲストのメモリ逼迫）。VM を立て直して回し直し、175 件すべて通った。

Linux aarch64 の単体の 1 回目は、VM のメモリ（4GB）でリンカが落ちた。6GB・1 ジョブで回し直して通した（試験の失敗ではない）。

### ビルド・静的解析（Linux x86_64）

| 対象 | 結果 |
|---|---|
| clippy `-D warnings`。`default` / `--no-default-features` / `full` / `full,epoll` / `full-container` / `full,api` / メッシュと、各 feature の単独（`--no-default-features --features <f>`）・既定 + 1 feature（`docs/artifacts/feature-check.sh`） | 全組み合わせで指摘 0 |
| macOS（`full-macos`）・Windows（`full-windows`）の clippy `--all-targets -D warnings` | 指摘 0 |
| `cargo fmt --check` | 差分なし |
| `#[allow(dead_code)]` | 追加なし |
| `tools/lint/check-messages.py`（ログ・エラーの文字列が英語でチケット番号なし。F-242）・`tools/release/check-docs.sh`（docs.rs・PyPI の文書。F-241） | 0 件 |

### 公開物の確認（v0.8.3）

crates.io・PyPI・ghcr.io から取得して、何も設定せずに動くことを確かめた（`docs/artifacts/v0.8.3_release_note.md`）。

- Rust: `.cargo/config.toml` 無しのプロジェクトで `veil-proxy = "=0.8.3"`（`full`・`api`）をビルドした。
  HTTP/1.1・h2c・HTTP/2（TLS）・HTTP/3・OpenAPI が応答し、アクセスログが英語で標準出力に出た。
- Python: `pip install veil-api==0.8.3` の HTTP/1.1・h2c・アクセスログ。`help()` が英語でチケット番号なし。
- コンテナ（`latest`・`glibc`・`musl-v0.8.3`・`glibc-v0.8.3`）: `docker run` だけで内蔵のページと `/quickstart.html` を返した。
  自分の `config.toml` と HTML をマウントした場合も確かめた。
