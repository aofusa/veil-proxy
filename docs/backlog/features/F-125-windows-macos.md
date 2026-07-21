# F-125: macOS / Windows 対応

## 目的

veil を macOS（x86_64 / aarch64、`universal2-apple-darwin`）向けに Docker
クロスビルドできるようにする。QEMU 実行・実機テストは不可のため、**クロス
ビルドが通ることをもってビルド対応とする**（設計:
[docs/artifacts/f125_windows_macos_design.md](../../artifacts/f125_windows_macos_design.md)）。
Windows（`*-pc-windows-msvc`、WSAPoll reactor）は当初は設計・build.rs の cfg
名発行のみを本チケットで扱い実装本体を後続に分離する計画だったが、v0.6.0 で
x86_64-pc-windows-msvc の WSAPoll reactor/Winsock ソケット層 + Job Object
セキュリティ、および aarch64-pc-windows-msvc クロスビルド（aws_lc_rs）まで
同チケット継続作業として完了した（下記「現状（実装済み・Windows 分）」参照）。

## 現状（実装済み・macOS 分）

- **build.rs**: `target_os = "macos"` を検出したら `veil_rt_reactor` +
  `veil_poller_kqueue`（既存の FreeBSD/OpenBSD kqueue reactor をそのまま再利用）
  を発行するよう `match` 分岐へ追加。`--features epoll` は Linux 専用のまま
  （macOS で指定するとエラー）。unsupported target_os のエラーメッセージへ
  `macos`/`windows` を許可ターゲットとして追記。
  `target_os = "windows"` は `veil_rt_reactor` + `veil_poller_wsapoll` の
  **cfg 発行のみ**（`cargo::rustc-check-cfg=cfg(veil_poller_wsapoll)` を追加。
  WSAPoll reactor 本体・Winsock ソケット層は未実装で後続作業）。
- **`reactor/tcp.rs`**（macOS cfg 適応）:
  - `accept4(2)` が無いため、Accept Future の `#[cfg(target_os = "macos")]`
    分岐で `accept(2)` + `fcntl(F_SETFL, O_NONBLOCK)` +
    `fcntl(F_SETFD, FD_CLOEXEC)` にフォールバック。
  - `MSG_NOSIGNAL` が無いため、ソケット生成時（`create_nonblocking_socket`、
    および accept 直後の新規 fd）に `SO_NOSIGPIPE` を設定し、
    `WriteFuture`/`SendMsgFuture` の send/sendmsg フラグを macOS では `0` に
    （他 OS は `MSG_NOSIGNAL` のまま）。
  - `SO_REUSEPORT` は既存の `not(target_os = "freebsd")` 分岐でそのまま
    macOS にも適用される（macOS の `SO_REUSEPORT` はカーネル分散をしないため
    thread-per-core の accept 分散が偏り得るが、動作はする。既存コメントに
    包含済み）。
- **`runtime/udp.rs`**: ソケット生成時（`new_nonblocking_dgram_socket`）に
  macOS のみ `SO_NOSIGPIPE` を設定（UDP は本質的に SIGPIPE を発生させないが
  TCP 側と方針を統一するため保守的に設定）。send/sendto はもともと flags=0
  のため変更不要。
- **`runtime/offload.rs`**: 非同期オフロード完了通知 fd の生成で、macOS には
  `pipe2(2)` が無いため `#[cfg(target_os = "macos")]` 分岐で `pipe(2)` +
  `fcntl(F_SETFL, O_NONBLOCK)` + `fcntl(F_SETFD, FD_CLOEXEC)` の 2 段構えへ
  フォールバック（Linux は eventfd のまま、FreeBSD/OpenBSD は pipe2 のまま）。
- **`http3_server.rs`**: `create_memfd_for_pem` の memfd 版 cfg は
  `any(target_os = "linux", target_os = "freebsd")` のまま据え置き、一時ファイル
  フォールバック版の cfg を `target_os = "openbsd"` 限定から
  `not(any(target_os = "linux", target_os = "freebsd"))`（= openbsd + macos +
  windows）へ拡張。`PemBackedFile` の `temp_path` フィールド/Drop unlink も同じ
  cfg へ揃えた。macOS はこの経路が http3 feature 有効時のみコンパイルされる
  （下記「通らなかった feature」参照）。
- **`reactor/kqueue.rs`**: 既存の `TryInto<i16>`/`TryInto<u16>` ジェネリクスに
  よる filter/flags 型吸収がそのまま macOS でも機能し、追加変更なしでビルドが
  通った（設計ドキュメント想定どおり）。
- **ネイティブセキュリティ = `sandbox_init(3)`（Seatbelt）**:
  `src/security.rs` に `#[cfg(target_os = "macos")]` モジュール
  `macos_sandbox` を追加。
  - `sandbox_init`/`sandbox_free_error` を libSystem のシンボルとして直接 FFI
    宣言（capsicum/pledge と同じパターン。追加リンク不要。deprecated だが
    現在も機能する）。
  - `SandboxPaths { static_roots, read_only, read_write }` から SBPL
    プロファイル文字列を生成する `build_profile`。**実機検証ができないため
    保守的な最小プロファイル**を採用: `(deny default)` を基点に
    `network*`/`file-read*` は無条件許可し、**書き込みのみ**
    設定から導出したログ/ディスクキャッシュディレクトリの `subpath` に限定する
    （静的ルート・証明書/鍵の読み取り専用パスは `file-read*` により実効的に
    許可済みだが、将来 tightening する際の参照点としてプロファイル内へ
    コメント出力する）。
  - `apply(profile)` で `sandbox_init` を呼び出し、エラー時は `errorbuf` を
    `sandbox_free_error` で解放してからエラーとして返す。
  - `config.rs::GlobalSecurityConfig` に `enable_sandbox_macos`（bool,
    default false）を追加。非 macOS ビルドではキー自体は受理し警告して無視
    （capsicum/pledge と同じ方針）。
  - `config.rs::collect_macos_sandbox_paths`（`#[cfg(target_os = "macos")]`）
    が `collect_unveil_paths`（OpenBSD）と同様に設定ファイルを軽量に再パースし、
    静的ファイルルート・証明書/鍵・アクセスログ/アプリログ・ディスクキャッシュ
    ディレクトリを収集する。
  - `entry.rs`: FreeBSD capability mode / OpenBSD pledge と対称に、全 TLS
    ワーカーの listener bind 完了をバリア（`AtomicUsize`）で待ってから
    `veil-macos-sandbox` 専用スレッドが `sandbox_init` を適用する。失敗時は
    `allow_security_failures` に従う。
- **`examples/config.toml`**: `enable_sandbox_macos` の説明を pledge/unveil
  節の直後に追記。
- **TLS プロバイダ = ring**（実装時に判明）: 当初 aws_lc_rs 想定だったが、
  `cargo-zigbuild` での universal2 クロスリンクで aws-lc-sys の手書きアセンブリ
  `.S.o`（例 `bignum_sub_p521.S.o`）を zig リンカが解釈できず
  `error: unknown cpu architecture: 538976304`（0x20202020）でリンク失敗する。
  `AWS_LC_SYS_NO_ASM=1` は release ビルドでは aws-lc-sys が
  `AWS_LC_SYS_NO_ASM only allowed for debug builds!` と panic するため使えない。
  → macOS を **ring** プロバイダへ切替（OpenBSD と同じ経路を `src/tls_provider.rs` /
  `Cargo.toml` target 別依存で macOS へ拡張）。ring は `cc` ベースで、aws-lc-sys の
  cmake/nasm 依存も不要になった。
- **packaging**: `packaging/scripts/build-cross.sh`（新設）が
  `messense/cargo-zigbuild` コンテナ内で
  `cargo zigbuild --release --target universal2-apple-darwin` を実行し、
  `veil-<version>-universal2-apple-darwin.tar.gz`（バイナリ + config.toml.default
  + www + INSTALL.txt）を `packaging/output/` へ出力する。

## macOS クロスビルド検証結果

Docker コマンド:

```
docker run --rm -v $(pwd):/io -w /io messense/cargo-zigbuild \
  cargo zigbuild --release --target universal2-apple-darwin \
    --features 'http2,mimalloc,compression,cache,metrics,websocket,rate-limit,buffering,admin,access-log,l4-proxy'
```

- **通った feature セット**: `http2,mimalloc,compression,cache,metrics,websocket,rate-limit,buffering,admin,access-log,l4-proxy`。
  ring プロバイダで **warning 0・14m43s** で universal2 バイナリ生成に成功
  （`file` 出力: `Mach-O universal binary with 2 architectures [x86_64] [arm64]`、
  約 17.5MB）。
- **試したが通らなかった / 未検証の feature**:
  - `http3`: quiche が macOS クロスで aws-lc-sys（BoringSSL 互換）ビルドを要求し、
    上記と同じ zig リンク問題に当たる可能性が高い。macOS の http3 は未対応とする
    （フォローアップ）。
  - `wasm`（wasmtime）: Cranelift のクロスコンパイル可否が未検証。
  上記 2 つは設計ドキュメントの想定どおり「まず http2 中心のセットで通し、
  その後 http3/wasm を試す」の順で本チケットでは前段のみ実施した。

## Linux 無回帰

- `cargo build --features full` / `cargo build` / `cargo build --no-default-features` /
  `cargo build --features "full,epoll"`: warning 0。
- `cargo clippy --features full --all-targets` /
  `cargo clippy --features "full,epoll" --all-targets`: warning/error 0。
- `cargo test --lib --features full`: 781 件 PASS。
- `cargo fmt --all -- --check`: 差分なし。
- `#[allow(dead_code)]` は追加していない（cfg 分岐のみで各ターゲット対応）。

## 受け入れ条件

- [x] macOS 向け `universal2-apple-darwin` の Docker クロスビルドが通る
      （http3/wasm を除く feature セット）。
- [x] Linux（io_uring/epoll 両バックエンド、feature 組み合わせ）に無回帰。
- [x] macOS ネイティブセキュリティ（sandbox_init/Seatbelt）を実装し、
      config キー・entry.rs 配線・examples/config.toml を整備。
- [x] packaging（`build-cross.sh`）で universal2 tar.gz を生成できる。
- [x] Windows（WSAPoll reactor 本体・Winsock ソケット層・Job Object）を
      x86_64-pc-windows-msvc 向けに実装（v0.6.0、`cargo xwin build`
      クロスビルド、TLS 暗号プロバイダは ring）。
- [x] aarch64-pc-windows-msvc クロスビルド対応（aws_lc_rs、cmake クロス
      ビルド可・ring は aarch64-windows 向け prebuilt asm 非対応のため）。
- [ ] macOS の http3/wasm feature 対応は未検証（フォローアップ）。
- [ ] Windows/macOS の QEMU・実機検証は未実施（クロスビルド成功のみを合格
      基準とする、フォローアップ）。

## 依存・リスク

- macOS 実機での動作検証は一切行っていない（Docker クロスビルドの成功のみで
  「ビルド対応」と判断）。kqueue reactor のランタイム挙動・`sandbox_init` の
  実効性・`SO_REUSEPORT` の接続分散偏りなどは実機での確認が必要。
- `sandbox_init(3)` は Apple により deprecated 指定されているため、将来の
  macOS バージョンで削除・無効化されるリスクがある（代替は App Sandbox
  entitlements だが、サーバプロセスとしての適用は別途調査が必要）。
- Windows 対応（WSAPoll reactor・Winsock ソケット抽象・Job Object）は規模が
  大きいため、本チケットとは別チケットで実施する。
