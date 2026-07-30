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
- **【2026-07-29 実機検証で追記】NetBSD は x86_64 も wasmtime 非対応と判明**（B-55 更新）。
  NetBSD 10.1 amd64（QEMU 実機）で `cargo build --no-default-features --features
  full-netbsd`（当時 wasm 込み）を実行すると `wasmtime-40.0.4/src/runtime/vm/sys/
  unix/signals.rs` の `compile_error!("unsupported platform")` で失敗した。
  `signals.rs` には NetBSD 向けの `ucontext` 分岐が x86_64/aarch64 とも存在しない
  （FreeBSD/OpenBSD は x86_64 分岐だけは存在する点で NetBSD よりまだマシ）。Pulley
  インタープリタへの切替でも回避不能（失敗は wasmtime 自身の build.rs によるホスト
  target_arch 判定の時点で起きるため、`Config::target` の選択より前の話）。
  対応として `full-netbsd`/`full-netbsd-vendor`（x86_64 向け）からも `wasm` を除外した
  （`full-netbsd-aarch64`/`full-netbsd-aarch64-vendor` は元々除外済みだったため変更不要）。
  詳細は `docs/backlog/bugs/B-55-wasmtime-no-bsd-aarch64.md` 参照。

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

## tools/qemu 環境の構築（2026-07-29、コード配線のみ完了・実 VM 未検証）

`docs/artifacts/f136_platform_design.md`「F-138: NetBSD 対応」節の tools/qemu
サブセクション（入手性の実地調査結果）に基づき、`tools/qemu/bsd-vm.sh` に
`netbsd` を OpenBSD 相当の第 3 の OS として組み込んだ。**実際の QEMU 起動・
provision・toolchain・build・e2e はコーディネーターが別途実施する**ため、以下は
実装内容と設計上の想定であり、実機検証で変わりうる。

### 実装したもの

- `tools/qemu/bsd-vm.sh`: `netbsd` を `case "${OS_NAME}"` の分岐に追加
  （`freebsd|openbsd` → `freebsd|openbsd|netbsd`）。既存の FreeBSD/OpenBSD の
  分岐は 1 つも変更していない（`elif`/新規 `else` ブロックとして追加）。
  - ポート割り当て: `netbsd-x86_64` = 2350/2351/2352、
    `netbsd-aarch64` = 2360/2361/2362（ssh/console/QMP）。
  - `_image_url`: x86_64 は `NetBSD-<ver>-amd64-live.img.gz`、aarch64 は
    `NetBSD-<ver>-evbarm-aarch64.iso`（`NETBSD_VER` 既定 `10.1`）。
  - `cmd_setup`: x86_64 は live image を DL して `qemu-img convert` で qcow2 化し、
    FreeBSD と同じ「base.qcow2 + 起動用オーバーレイ」構成にする（cloud-init 相当が
    無いため seed は作らない）。aarch64 は install ISO を DL し、OpenBSD の
    miniroot と同様に空のターゲット qcow2 を用意する。
  - `cmd_provision`: x86_64 はシリアルへ root ログインして SSH 鍵注入
    （`netbsd-provision.py`、FreeBSD の `--mode login` と同じ発想）。aarch64 は
    `sysinst` を自動操作してインストール後（`netbsd-autoinstall.py`）、**ISO を
    外して再起動してから** `netbsd-provision.py` で鍵注入する 2 段構成
    （sysinst 完了直後は ISO が bootindex=0 のままなので reboot するとインストーラ
    に戻ってしまうため）。
  - `cmd_toolchain`: `pkgin install rust-bin cmake llvm protobuf gmake bash curl
    git nasm pkgconf`。**`rust` ではなく `rust-bin`** を明示指定
    （ソースビルドは QEMU 上で数時間かかる想定のため）。`pkgin` 自体が無い場合は
    `pkg_add -v pkgin` で bootstrap する分岐を入れた。`PKG_PATH` は
    `https://cdn.NetBSD.org/pub/pkgsrc/packages/NetBSD/<arch>/<NETBSD_PKG_VER>/All/`
    （既定 `NETBSD_PKG_VER=10.0`。実際は `10.0_2026Q2` 等へ 302 リダイレクトされる
    ことを `curl -sIL` で確認済み）。**`pkgconf` は `system-tls`（既定の NetBSD
    packaging 構成）に必須**であることを 2026-07-29 の実機検証で確認した:
    NetBSD base には OpenSSL 3.0.12 と `/usr/lib/pkgconfig/openssl.pc` が既にあるが
    `pkg-config` コマンド自体は base に無く、無いと `openssl-sys` のビルドが
    `Could not find directory of OpenSSL installation` で失敗する。この一覧には
    元々 `pkgconf` を含めてあったため追加対応は不要。
  - `_default_features`: `netbsd) echo "full-netbsd${suffix}"` を追加。
  - `_guest_env_prefix`: NetBSD 用に `LIBCLANG_PATH` の探索先を `/usr/pkg` に、
    `PATH` に `/usr/pkg/{bin,sbin}` を追加する分岐を追加（pkgsrc は `/usr/pkg`
    配下にインストールされ、非対話 ssh セッションの既定 PATH に含まれないため）。
  - `_write_boot`: NetBSD aarch64 の install フェーズ用ドライブ構成
    （ISO を virtio-scsi 経由の CD-ROM、ターゲットディスクを virtio-blk）を追加。
  - `cmd_reset`: cloud-init シード書き込みを `[[ "${OS_NAME}" == "freebsd" ]]` の
    条件に変更（従来は無条件呼び出しだったが、NetBSD x86_64 も `base.qcow2` を
    持つため、reset 時に不要な cloud-init シードを作らないようにした）。
- `tools/qemu/netbsd-provision.py`（新規）: x86_64 live image 用のシリアル
  ログイン provision。ブートメニューでの `consdev com0` 切り替え試行 → 失敗しても
  続行 → ログインプロンプト待ち → SSH 鍵注入・`sshd=YES`・sshd 再起動、という
  ベストエフォート実装。**実機コンソールの文言では未検証**。
- `tools/qemu/netbsd-autoinstall.py`（新規）: aarch64 の `sysinst` を
  シリアルから自動操作する。OpenBSD の `autoinstall(8)` と異なり応答ファイル
  方式が無いため、一般的な sysinst の操作順序（言語選択 → メインメニュー →
  ディスク選択 → GPT 全体パーティション → CD-ROM からのセット取得 → 確認 →
  インストール）をキー送出で推定実装。**メニュー文言・キー割り当てはリリースに
  より変わりうるため実機での調整が前提**。
- `packaging/bsd/netbsd/veil.rc`（新規）: NetBSD の rc.d サービススクリプト。
  NetBSD には FreeBSD の `daemon(8)` に相当するものが標準に無いため、
  `command_args` の末尾に `& echo $! > pidfile` を付けて自前でバックグラウンド化
  する構成にした（BSD 系 rc.d スクリプトでよく使われるパターン）。
- `packaging/scripts/build-bsd.sh`: `--os` に `netbsd` を追加、`--all` の対象に
  `netbsd` を追加、rc.d のインストール先パスの分岐を `${OS}` 汎用化（従来
  `openbsd` 固定だった箇所を `${BSD_ASSETS}/${OS}/veil.rc` に変更）、
  `INSTALL.txt` に NetBSD 用の手順（`rcctl` が無いので `/etc/rc.conf` を直接
  編集し `/etc/rc.d/veil start`）を追加。
- `packaging/README.md` / `tools/qemu/README.md`: NetBSD 対応の記述を追記
  （使い方・既知の不確実点・検証状況テーブル）。

### x86_64 で想定される手順（実機未検証）

```bash
tools/qemu/bsd-vm.sh netbsd x86_64 setup       # live.img.gz DL + qcow2 化
tools/qemu/bsd-vm.sh netbsd x86_64 provision   # 起動 → シリアルログイン → 鍵注入
tools/qemu/bsd-vm.sh netbsd x86_64 toolchain   # pkgin install rust-bin cmake llvm ...
tools/qemu/bsd-vm.sh netbsd x86_64 build       # --features full-netbsd
tools/qemu/bsd-vm.sh netbsd x86_64 e2e
tools/qemu/bsd-vm.sh netbsd x86_64 fetch
```

**autoinstall スクリプトは不要と見込んでいる**（live image がそのまま起動可能な
ため）。ただし live image のパーティションサイズ・root パスワードの扱い・
ネットワーク自動設定の有無は未確認であり、これらが期待どおりでない場合は
OpenBSD 同様の autoinstall 相当の仕組みが必要になる可能性がある。

### aarch64 で想定される手順（実機未検証）

```bash
tools/qemu/bsd-vm.sh netbsd aarch64 setup       # install ISO DL + 空ディスク作成
tools/qemu/bsd-vm.sh netbsd aarch64 provision   # ISO 起動 → sysinst 自動操作 →
                                                 # ISO 無しで再起動 → 鍵注入
tools/qemu/bsd-vm.sh netbsd aarch64 toolchain
tools/qemu/bsd-vm.sh netbsd aarch64 build       # --features full-netbsd-aarch64（wasm 抜き）
tools/qemu/bsd-vm.sh netbsd aarch64 e2e
tools/qemu/bsd-vm.sh netbsd aarch64 fetch
```

**`netbsd-autoinstall.py` が必要**（ISO 配布のみのため）。sysinst の実際の画面
遷移・文言は未確認であり、`netbsd-autoinstall.py` の expect パターンは高確率で
調整が必要になる。`tools/qemu/console-dump.py` で実際の出力を確認しながら
イテレーションすることを推奨する。x86_64 と aarch64 で pkgsrc の Rust バージョン
が異なる（`rust-bin-1.96.0` / `rust-bin-1.91.1`）ため、aarch64 側で veil の
MSRV を満たすか toolchain 実行時に要確認。

### 確認したコマンド（本チケットの範囲、VM 未起動）

```bash
bash -n tools/qemu/bsd-vm.sh                          # 構文エラー無し
tools/qemu/bsd-vm.sh netbsd x86_64                    # 引数不足で usage 表示（想定どおり）
tools/qemu/bsd-vm.sh freebsd x86_64                   # 既存動作に変化なし
bash -n packaging/scripts/build-bsd.sh                # 構文エラー無し
python3 -m py_compile tools/qemu/netbsd-provision.py  # 構文エラー無し
python3 -m py_compile tools/qemu/netbsd-autoinstall.py # 構文エラー無し
curl -sIL <各 URL>                                     # すべて HTTP 200（302 経由）
```

shellcheck は本環境に未導入のため未実施（コーディネーター環境で確認推奨）。


## 検証状況（2026-07-30 時点）

| 対象 | 状態 |
|---|---|
| NetBSD x86_64 | **実機ビルド成功**（`full-netbsd`、release 35分34秒、warning 0）。E2E 実施 |
| NetBSD aarch64 | **未検証（今回のスコープ外）**。`tools/qemu/bsd-vm.sh netbsd aarch64` の配線と
  `netbsd-autoinstall.py` は用意済みだが、実 ISO に対する sysinst のキー送出は未調整。
  aarch64 は wasm 非対応（wasmtime）に加え検証時間が長いため、今回は見送った |

### x86_64 で必要だった前提パッケージ（実機で確定）

```
pkgin install rust-bin cmake llvm clang libressl protobuf gmake bash curl git nasm pkgconf
```

- `rust-bin` — ソースの `rust` は QEMU 上で数時間かかる
- **`clang`** — pkgsrc では `llvm` と別パッケージ。`llvm` だけでは `/usr/pkg/lib/libclang.so`
  が入らず bindgen が `Unable to find libclang` で失敗する（FreeBSD/OpenBSD の llvm とは構成が違う）
- `pkgconf` — base に `pkg-config` コマンドが無く `openssl-sys` が失敗する
- `libressl` — `system-tls` 利用時のみ。ただし **`system-tls` は HTTP/3 と併用不可**
  （F-137/F-142 参照）なので、既定の `full-netbsd`（vendored）では不要
