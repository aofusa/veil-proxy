# プラットフォーム対応・ランタイムバックエンド

[← ドキュメント目次](README.md) · [English](../platforms.md)

## プラットフォーム対応・ランタイムバックエンド（F-120 / F-125）

データプレーンのランタイムバックエンドは**コンパイル時**に選択される（動的ディスパッチ・
ホットパスコストなし）。デフォルト（Linux io_uring）は不変。

| プラットフォーム | ランタイムバックエンド | ネイティブセキュリティ | kTLS | 備考 |
|---|---|---|---|---|
| **Linux（デフォルト）** | io_uring（`src/runtime/uring/`） | seccomp + Landlock + CBPF | ✅（Linux 5.15+） | デフォルト features 不変・性能非劣化 |
| **Linux `--features epoll`** | epoll readiness reactor（`src/runtime/reactor/`） | seccomp（epoll 系許可・io_uring 系除外）+ Landlock | ✅ | io_uring 非対応ホスト向けフォールバック |
| **FreeBSD（x86_64/aarch64）** | kqueue readiness reactor（`--features aio` で POSIX AIO 経路にも切替可・F-127） | capsicum（`cap_rights_limit` / `cap_enter`）+ jail | ✅（FreeBSD 13.0+、`TCP_TXTLS_ENABLE`/`TCP_RXTLS_ENABLE`; F-126）。**ただし大きな応答では有効化しないこと**: FreeBSD の software kTLS は TLS レコードごとにカーネルワーカースレッドへ暗号処理をディスパッチするため、54KB 応答の実測でスループットが **26% 低下**し 1.2 GB/s で直列化により張り付く（既定は false。詳細は [docs/perf/README.md](../../perf/README.md)） | `[security] enable_capsicum` / `capsicum_capability_mode` / `jail_name`。TLS 証明書ホットリロード（H1/H2・HTTP/3 とも）は capsicum capability mode 下でも動作する（F-136）: cert/key の親ディレクトリ fd を `cap_enter` 前に確保し `openat`/`fstatat`（`O_RESOLVE_BENEATH`）で読む。`http3`（quiche）は rustls との `aws-lc-sys` 共有をやめ **`boringssl-boring-crate`**（外部 `boring` crate）へ切替済みで、パスを一切介さない in-memory `SSL_CTX` API で証明書を再構築できる |
| **OpenBSD（x86_64/aarch64）** | kqueue readiness reactor | pledge + unveil | ✗（ユーザ空間 rustls） | `[security] enable_pledge` / `enable_unveil`。TLS は rustls の **ring** プロバイダを使用（aws-lc-rs は OpenBSD でハンドシェイク未完・F-122）。**WASM は Pulley インタープリタで動作**（B-52）: OnDemand インスタンスアロケータ + `MAP_STACK` 付きファイバスタックと併用する（wasmtime のプーリングアロケータは `with_host_stack` を黙って無視し、OpenBSD は SP が `MAP_STACK` 領域外だとプロセスを殺すため）。Pulley はネイティブコードを生成しないので `wxallowed` なファイルシステムが不要（速度はインタープリタ相当）。aarch64 では wasmtime 自体も vendoring 版に切り替わる（下記 NetBSD の行・B-55 参照。x86_64 と挙動は同じ）。静的配信/プロキシとも HTTPS 200 検証済み |
| **NetBSD（x86_64/aarch64）** | kqueue readiness reactor | **chroot(2) + 特権降格のみ（pledge/unveil 相当は無い）**（F-140） | ✗（ユーザ空間 rustls） | `[security] chroot_dir`（opt-in、`chroot(2)` + `chdir("/")`。`drop_privileges_user`/`drop_privileges_group` による setuid/setgid より前に適用）。NetBSD には OpenBSD の pledge/unveil に相当するランタイム API が無い（Veriexec はカーネル設定・ロード時整合性検証機構でプロセス自身が呼べる syscall フィルタではなく、`secmodel_securelevel` はシステム全体の起動時設定）。veil は非対応であることを起動時ログで正直に報告する（`security::netbsd::report_security_support`）。TLS は OpenBSD と同じ rustls **ring** + quiche **boringssl-boring-crate**。**NetBSD（アーキテクチャ不問）・FreeBSD aarch64・OpenBSD aarch64 でも Proxy-Wasm が使える**（B-55 解消）: crates.io の wasmtime 40 のシグナルベーストラップ実装にはこの 3 ターゲット向けの `ucontext` 分岐が一切無く、NetBSD は x86_64 ですら wasmtime 自体の build.rs 判定で `signals.rs` が `error: unsupported platform` になる（Pulley への切替は `Config::target` の選択なので、それより前に起きるこの失敗は回避できない）。対応として crates.io wasmtime 40.0.4 を `third_party/wasmtime`（パッケージ名のみ `veil-wasmtime` に変更、`[lib] name` は `wasmtime` のまま据え置きのため `src/` の import は無変更。詳細は `third_party/wasmtime/README.veil.md`）として vendoring し、`build.rs` に 2 行の差分（この 3 ターゲットでのみ `has_native_signals = false` を強制）を加えたものを Cargo のターゲット別依存で選択させる。それ以外の全プラットフォーム（Linux/Windows/macOS/FreeBSD x86_64/OpenBSD x86_64）は crates.io の wasmtime 40.0.0 を無変更のまま使う。この 3 ターゲットは常に **Pulley インタープリタ**で WASM を実行する（ネイティブ JIT を使わないためシグナルベースの trap がそもそも不要）。`full-freebsd`/`full-openbsd`/`full-netbsd` はいずれも `wasm` を含む（feature セットはアーキ非依存。vendoring 版 wasmtime の切り替えは feature ではなく Cargo のターゲット別依存で行う）。**NetBSD はさらに `paxctl +m <バイナリ>` を実行しないと WASM フィルタが動かない**（B-60）: NetBSD は PaX MPROTECT をシステム全体で強制しており（`security.pax.mprotect.enabled`/`.global` = 1）、Pulley インタープリタ実行であっても wasmtime のランタイム `mmap`/`mprotect` が `EACCES` で失敗するため、`paxctl(8)` でバイナリの MPROTECT 制限を明示的に解除する必要がある。`tests/e2e_setup.sh`（NetBSD 実行時に自動適用）・`tools/qemu/bsd-vm.sh`（`cmd_build` がビルド直後のゲストバイナリへ自動適用）・`packaging/scripts/build-bsd.sh`（NetBSD ホストでのパッケージ化時は自動適用、それ以外はインストール手順に明記）の 3 箇所へ組み込み済み。実機 NetBSD 10.1（x86_64・evbarm-aarch64 とも）で検証済み |
| **macOS（x86_64/aarch64、universal2）** | kqueue readiness reactor（FreeBSD/OpenBSD/NetBSD と共通実装を再利用） | `sandbox_init`（Seatbelt） | ✗（ユーザ空間 rustls） | `[security] enable_sandbox_macos`。TLS は rustls の **aws_lc_rs** プロバイダ、`http3`（quiche）は内蔵 BoringSSL を使用（F-131）。`docker/Dockerfile.macos`（`cargo zigbuild --target universal2-apple-darwin --features full`）でクロスビルドし、実機で動作確認済み |
| **Windows（x86_64-pc-windows-msvc / aarch64-pc-windows-msvc）** | WSAPoll readiness reactor（`src/runtime/reactor/wsapoll.rs`、`src/runtime/reactor/tcp/windows.rs`、Winsock） | Job Object（best-effort） | ✗（ユーザ空間 rustls） | `[security] enable_job_object_windows`。TLS は両 arch とも rustls の **aws_lc_rs** プロバイダ、`http3`（quiche）は内蔵 BoringSSL を使用（F-131）。`docker/Dockerfile.windows`（`cargo xwin build --target <target> --features full`。`packaging/scripts/build-cross.sh --target windows` で両 arch を一括ビルド）でクロスビルドし、実機で動作確認済み |

- バックエンドは `build.rs` 発行の cfg（`veil_rt_uring` / `veil_rt_reactor`、
  `veil_poller_epoll` / `veil_poller_kqueue`）で選択され、公開ランタイム API パス
  （`runtime::tcp` / `runtime::executor` / `runtime::timer` 等）は全バックエンドで不変。
- `--features epoll` は Linux 専用（他ターゲットでは build.rs がエラー。BSD/macOS は
  kqueue が自動選択）。非対象 OS のセキュリティ設定キーは受理し警告して無視する。
- **aarch64-linux**: `docker/Dockerfile.{glibc,musl}.aarch64` でクロスビルド（QEMU
  user-mode 検証済み。QEMU は io_uring 非対応のため QEMU 実行は `epoll` ビルドを使用）。
- **FreeBSD/OpenBSD** は対応する QEMU VM 内でビルドする。`tools/qemu/bsd-vm.sh <os> <arch>`
  が FreeBSD/OpenBSD × x86_64/aarch64 の 4 通りについて、VM 作成 → ビルド →
  `tests/e2e_setup.sh test` → バイナリ取得までを一括で扱う（x86_64 ゲストはホストに
  `/dev/kvm` があれば KVM 加速される）。tar.gz + rc.d/jail.conf のパッケージングは
  `packaging/scripts/build-bsd.sh` 参照。
  **NetBSD 対応（F-140）はコード側は完成しているが `bsd-vm.sh`/`build-bsd.sh` への
  組み込みは未着手**（QEMU VM 構築・E2E は別チケット
  `docs/backlog/features/F-140-netbsd-support.md` で扱う）。本開発環境には NetBSD 向け
  C クロスコンパイラも無いため、`cargo check --target x86_64-unknown-netbsd` は
  `ring`/`boring` のネイティブビルドスクリプトの時点で失敗し veil 自身のコードへは
  到達できていない。
  **FreeBSD には Docker クロスビルド経路が無い** — `docker/Dockerfile.freebsd` と
  `build-cross.sh --target freebsd` はリンク段で失敗する（aws-lc-sys の s2n-bignum
  アセンブリが FreeBSD クロス構成で 1 つも組み立てられず
  `undefined symbol: curve25519_x25519_byte` などが多数出る、
  `docs/backlog/bugs/B-49-...` 未解決）ため削除した（チケットに削除の経緯を記録）。
  **QEMU VM 内ネイティブビルドが FreeBSD の唯一の公式経路**であり、x86_64 /
  aarch64 とも同じ手順を使う（`aarch64-unknown-freebsd` はさらに Rust Tier 3 で
  prebuilt std が無いため、そもそも Docker を使えない）:
  `tools/qemu/bsd-vm.sh freebsd <arch> build` → `fetch` →
  `packaging/scripts/build-bsd.sh --os freebsd --arch <arch> --binary <パス>`。
- **FreeBSD POSIX AIO（`--features aio`、F-127）**: ビルド時オプトイン切替。**推奨しない**（FreeBSD 専用。
  他ターゲットで指定すると `epoll` と同様 build.rs がエラーにする）。既定の kqueue
  readiness 経路の代わりに `TcpStream::read`/`write` を `aio_read(2)`/`aio_write(2)` の
  完了通知ベースへ切り替える。完了は同じ kqueue ループへ `EVFILT_AIO`
  （`aio_sigevent.sigev_notify = SIGEV_KEVENT`）として届く。`EAGAIN`（AIO デーモンプール/
  キュー上限）時は当該 I/O だけ readiness 経路へフォールバックする。`--features full` には
  含まれず、**`full-freebsd` からも除外した（B-63）**: FreeBSD 14.3
  aarch64 実測で readiness 経路より一貫して劣るため。POSIX AIO は 1 I/O あたり 3 syscall
  （submit + `aio_error` + `aio_return`）を要し（readiness は 1）、小レスポンスの
  HTTP/1.1 TLS で 105,520 rps 対 187,374 rps（**+77.6%**）、L4 TCP で約 37k 対約 202k rps と
  大差がつく一方、54KB の大きなレスポンスでは有意差が無い。さらに HTTP/2 で小さな
  レスポンスを高並行に返すとサーバが完全に停止する（B-63）。設計・検証結果は
  `docs/backlog/bugs/B-63-freebsd-aio-h2-stall.md`、
  `docs/artifacts/f127_freebsd_aio_design.md`、
  `docs/backlog/features/F-127-freebsd-aio.md` を参照。
- **macOS（F-125/F-131）**: クロスビルドのみ対応。Docker（`docker/Dockerfile.macos`、
  `messense/cargo-zigbuild` ベース）でビルドする
  （`packaging/scripts/build-cross.sh --target macos` 参照）。TLS 暗号は
  **aws_lc_rs** プロバイダを使用する（`src/tls_provider.rs` 参照）。macOS には
  `accept4`/`MSG_NOSIGNAL`/`pipe2`/`SOCK_NONBLOCK|SOCK_CLOEXEC` が無いため、
  `reactor/tcp.rs`・`runtime/udp.rs` は素の `socket`/`accept` + `fcntl`・`SO_NOSIGPIPE` へ、
  `runtime/offload.rs` は `pipe` + `fcntl` へそれぞれフォールバックする。
  `build-cross.sh --target macos` の既定は `--features full`（`http3`・`wasm` を含む）で、
  実機で動作確認済み（F-131）。
- **Windows（F-125/F-131、v0.6.0）**: クロスビルドのみ対応。Docker
  （`docker/Dockerfile.windows`、`messense/cargo-xwin` ベース）で
  x86_64-pc-windows-msvc / aarch64-pc-windows-msvc を個別にビルドする
  （`packaging/scripts/build-cross.sh --target windows` 参照）。TLS 暗号プロバイダは
  両 arch とも **aws_lc_rs**、`http3`（quiche）は内蔵 BoringSSL を使用する。
  既定は `--features full`（`http3`・`wasm`・`l4-proxy` を含む）で、実機で動作確認済み。
  `ktls` は Linux/FreeBSD 専用のため対象外。
- **TLS 暗号プロバイダ / quiche 暗号バックエンドのターゲット分割（F-122/F-131/F-136/F-140）**:
  rustls のプロバイダは **OpenBSD/NetBSD のみ `ring`**（NetBSD は未検証だが OpenBSD と
  同じ Tier 3 サポート不足の懸念があるため保守的に合わせた）、それ以外
  （Linux/FreeBSD/macOS/Windows）は `aws_lc_rs`（`src/tls_provider.rs` と `Cargo.toml` の
  target 別依存を一致させること）。
  `http3` の quiche は **Linux でのみ `aws-lc-sys` を共有**し（memfd 経由の従来証明書ロード、
  無変更）、**FreeBSD/macOS/Windows/OpenBSD/NetBSD では `boringssl-boring-crate`**（外部
  `boring` crate）を使う。FreeBSD は F-136（capsicum capability mode 下での証明書ホットリロード）で
  `aws-lc-sys` 共有から切り替えた: `Config::with_boring_ssl_ctx_builder` という
  パスを一切介さない in-memory `SSL_CTX` 構築 API が `boringssl-boring-crate` でしか
  提供されないため。`aws-lc-sys`（`NO_PREFIX=1`）と外部 `boring` crate を同一バイナリに
  同居させるとリンク時に重複シンボルエラーになることを実験で確認済み
  （`docs/artifacts/f136_platform_design.md`）。この切り替えが `AWS_LC_SYS_NO_PREFIX` であり、
  値は [`.cargo/config.toml`](../../../.cargo/config.toml) の `[env]` のみで設定する（B-47）。
