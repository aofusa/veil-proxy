# F-140: NetBSD 対応

## 目的

OpenBSD 対応（F-120 Phase 5 / F-122 / B-52 / F-136 / F-137）をテンプレートに、NetBSD を
新規サポート対象 OS として追加する。設計は
[docs/artifacts/f136_platform_design.md](../../artifacts/f136_platform_design.md) の
「F-138: NetBSD 対応」節（チケット番号は F-138 が別用途 = 現行
[F-138-proxy-wasm-buffer-maptype-gaps.md](F-138-proxy-wasm-buffer-maptype-gaps.md) で
既に使われているため、実装チケットは F-140 とした）を参照。

本チケットは **コード側のみ**。QEMU での実ビルド・E2E 検証は別チケット（`tools/qemu`
整備を含む）で行う。

## 改修内容

### ランタイム / build.rs

- `build.rs`: `target_os = "netbsd"` を **kqueue reactor**（`veil_rt_reactor` +
  `veil_poller_kqueue`）へ割り当て（`"freebsd" | "openbsd" | "macos"` の分岐に追加）。
- `SYSTEM_TLS_ALLOWED_TARGET_OSES` に `"netbsd"` を追加。
- `src/runtime/reactor/kqueue.rs`: NetBSD の `struct kevent` は歴史的な BSD 定義
  （FreeBSD/OpenBSD/macOS の `filter: i16` / `flags: u16`）から拡張されており、
  `filter`/`flags` とも `uint32_t`、`udata` は `intptr_t` ではなく `void *`。
  `make_kevent()` は `filter`/`flags` の代入先フィールド型が変わるため、
  `TryInto<i16>`/`TryInto<u16>` 固定の既存実装は NetBSD では型不一致でコンパイルできない。
  `#[cfg(target_os = "netbsd")]` で `TryInto<u32>` 版を別途用意して吸収した
  （FreeBSD/OpenBSD/macOS 側の実装は 1 行も変更していない）。`udata` は本実装では
  一切設定しない（`mem::zeroed()` のままゼロ/NULL）ため、型差はここでは実害が無い。

### TLS

- rustls: **ring**（OpenBSD と同じ）。`src/tls_provider.rs` の cfg を
  `any(target_os = "openbsd", target_os = "netbsd")` に拡張。
- quiche: **BoringSSL**（`boringssl-boring-crate` 経由の外部 `boring` crate）。
  `Cargo.toml` の `[target.'cfg(target_os = "openbsd")'.dependencies]` を
  `[target.'cfg(any(target_os = "openbsd", target_os = "netbsd"))'.dependencies]` へ
  広げることで、quiche/boring/wasmtime(pulley) の依存を NetBSD にも自動的に適用した
  （新規に重複ブロックを作らず、OpenBSD 側の記述をそのまま共有）。
- `.cargo/config.toml` に `AWS_LC_SYS_NO_PREFIX_{x86_64,aarch64}_unknown_netbsd = "0"`
  を追加（OpenBSD と同じ扱い）。
- テスト用 TLS スタック（`[target.*.dev-dependencies]` の rcgen/tokio-rustls/
  hyper-rustls/tonic、B-51 の教訓）も同じ cfg 拡張で NetBSD に適用。
  `tests/test_backends/Cargo.toml`・`tests/test_backends/.cargo/config.toml`・
  `tests/grpc_server/.cargo/config.toml` も同様に対応。

### wasm

- `src/wasm/registry.rs` の Pulley 判定（2 箇所）を
  `cfg!(target_os = "openbsd") || cfg!(target_os = "netbsd") || config.interpreter`
  に拡張。NetBSD はネイティブ JIT 実行が未検証のため、OpenBSD と同様に安全側で
  Pulley インタープリタへ倒す。
- **`openbsd_stack`（`MAP_STACK` 付きファイバスタック）・`OnDemand` アロケータ強制は
  NetBSD には追加しない**。`MAP_STACK` の強制は OpenBSD 6.4+ カーネル固有の制約で
  NetBSD には無いため、NetBSD は Pulley + 通常の Pooling アロケータで動作する
  （`create_engine` の `#[cfg(not(target_os = "openbsd"))]`/`#[cfg(target_os =
  "openbsd")]` 分岐はそのまま。NetBSD は `not(target_os = "openbsd")` 側＝Pooling を通る）。
- `src/wasm/types.rs`: `interpreter = false` が NetBSD でも無視される旨の警告を追加。
- aarch64 は wasmtime のシグナルベーストラップが BSD×aarch64 未対応（B-55）のため、
  `full-netbsd-aarch64`/`full-netbsd-aarch64-vendor` は wasm 抜き。

### セキュリティ（`src/security.rs`）

NetBSD には pledge/unveil に相当するランタイム API が無い（Veriexec はカーネル設定・
ロード時整合性検証機構であり、プロセスがランタイムで呼べる syscall フィルタ/パス
制限 API ではない。secmodel_securelevel はシステム全体の起動時設定）。

実装した `netbsd` モジュール（`#[cfg(target_os = "netbsd")]`）:

- `chroot_to(dir: &Path) -> io::Result<()>`: `chroot(2)` + `chdir("/")`。
  設定キー `chroot_dir`（`GlobalSecurityConfig`、NetBSD 専用・非対象 OS は受理して
  警告のみ）で指定時、`entry.rs` で `drop_privileges`（setuid/setgid）より**前**に
  適用する（先に setuid すると多くの実装で `chroot(2)` 自体が `EPERM` になるため）。
- `report_security_support()`: 起動時ログで対応範囲を正直に報告する
  （pledge/unveil 相当が無いことを明記）。

**特権降格（setuid/setgid/setgroups）と rlimit（`RLIMIT_NOFILE` 等）は NetBSD 固有の
実装を追加していない**: `src/system.rs::drop_privileges` は既に `#[cfg(unix)]` で
POSIX 共通実装のため NetBSD でもそのまま動作し、`crate::system::raise_nofile_limit`
も同様。「pledge 相当がある」かのような実装・記述はしていない。

### feature セット（Cargo.toml）

`full-openbsd` / `full-openbsd-vendor` / `full-openbsd-aarch64` /
`full-openbsd-aarch64-vendor` と同じ内容で `full-netbsd` / `full-netbsd-vendor` /
`full-netbsd-aarch64` / `full-netbsd-aarch64-vendor` を追加。`openbsd-vendor-tls` と
同一内容の `netbsd-vendor-tls`（`quiche?/boringssl-boring-crate` + `dep:boring`）も
追加した（依存内容は同じだが、`full-openbsd-vendor`/`full-netbsd-vendor` を独立に
維持するためフォワーディング feature 自体は分離）。

### ドキュメント

- README.md / docs/readme/README.ja.md / AGENTS.md にサポート OS として NetBSD を追記
  し、セキュリティ機能は chroot + 特権降格のみで pledge/unveil 相当が無いことを明記。
- packaging/README.md に NetBSD ターゲットを追記。

## 検証（本チケットの範囲）

- `cargo build --features full`（Linux）: warning ゼロで成功、`Cargo.lock` 差分なし
  （`cargo tree --features full` に変化が無いことを確認）。
- `cargo test --lib --features full`: 既存 795 件全て成功（回帰なし）。
- `cargo clippy --features full --all-targets -- -D warnings`: warning/error ゼロ。
- `cargo build --no-default-features`: 成功。
- `cargo check --target x86_64-unknown-netbsd --no-default-features --features
  full-netbsd`: **本環境には NetBSD 向け C クロスコンパイラ（`*-netbsd-gcc`）が
  無く**、`ring`/`boring`（BoringSSL の cmake ビルド）の native ビルドスクリプルで
  失敗するため最後まで到達できなかった。Rust 側の依存グラフ解決・feature 解決、
  および多数の依存クレート（cranelift/wasmtime 系列を含む）のコンパイルはエラー無く
  進行しており、`veil` 自身のコード（kqueue.rs の netbsd 分岐等）へ到達する前の
  ネイティブビルド依存で止まっている。実機/QEMU 環境でのクロスツールチェイン整備後の
  最終確認は別チケット（`tools/qemu/bsd-vm.sh` への netbsd 追加）で行う。

## 未対応・既知の限界

- NetBSD のセキュリティ機能は chroot(2) + 特権降格 + rlimit のみ。pledge/unveil
  相当のプロセス自身による syscall フィルタ・パスホワイトリストは提供できない。
- QEMU での実ビルド・E2E は未実施（別チケット）。
- NetBSD 上での実機動作は未検証（rustls=ring・quiche=boring の選択は OpenBSD の
  実証結果に基づく保守的な類推であり、NetBSD 固有の検証はまだ無い）。
