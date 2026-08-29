# tools/qemu — full-system QEMU 検証環境（プラットフォーム×arch ビルド・動作確認）

veil を **各プラットフォーム×arch の実カーネル上**でビルド・E2E・`tools/perf` 検証する
ための QEMU 環境を用意するスクリプト群。

ホストに `qemu-system-*` / UEFI ファーム / cloud image ツールが無く sudo も使えない環境を
想定し、これらを内包した Docker ヘルパイメージ（`helper/Dockerfile`）経由で起動する。
ヘルパには **aarch64 ゲスト（qemu-system-arm + AAVMF）と x86_64 ゲスト
（qemu-system-x86 + OVMF）の両方**が入っており、x86_64 ゲストはホストに `/dev/kvm` が
あれば **KVM 加速**される（実用速度で in-VM フルビルド + E2E が回る）。

## プラットフォーム×arch 検証マトリクス

| プラットフォーム | x86_64 | aarch64 | 手段 |
|---|---|---|---|
| Linux（io_uring/epoll） | ネイティブ/Docker で直接 | Docker クロスビルド + full-system QEMU で E2E（`linux-aarch64-e2e.sh`）。**KVM 不可ホストでは TCG が実用不能**（下記制約） | `aarch64-vm.sh` / `run-e2e-aarch64.sh` / `linux-aarch64-e2e.sh` |
| FreeBSD | **VM 内ネイティブビルド**（KVM で実用速度。実測 ~30 分）。Docker クロスビルドは B-49 未解決で使えない | Rust Tier 3 のため **VM 内ネイティブビルド**（TCG のため低速） | `bsd-vm.sh freebsd {x86_64,aarch64}` |
| OpenBSD | miniroot から autoinstall した VM で **ネイティブビルド**（KVM で実用速度） | 同左（TCG のため低速） | `bsd-vm.sh openbsd {x86_64,aarch64}` |
| NetBSD（F-140） | 起動可能な live image を使い **VM 内ネイティブビルド**（`rust-bin` で数分程度に短縮） | `evbarm-aarch64` 向けの起動可能な `gzimg/arm64.img.gz` を使い **VM 内ネイティブビルド**（TCG のため低速。x86_64 と同じ経路） | `bsd-vm.sh netbsd {x86_64,aarch64}` |
| macOS / Windows | ネイティブ実行ホストが無く **Docker クロスビルドのみ**（`docker/Dockerfile.{macos,windows}` / `packaging/scripts/build-cross.sh`） | 同左 | — |

**なぜ Linux/macOS/Windows に QEMU が要らないか**: Linux x86_64 はホストそのもの、
macOS/Windows は Docker（cargo-zigbuild / cargo-xwin）で完結するクロスビルドのみを
合格基準としているため。Linux aarch64 だけは io_uring を**実カーネル**で確認したいので
QEMU を使う。

---

## BSD 統合ヘルパ `bsd-vm.sh`（FreeBSD / OpenBSD / NetBSD × x86_64 / aarch64）

`packaging/` が配布する BSD バイナリを**実 OS 上でビルド・E2E 検証**し、成果物を
host 側へ取り出すための統合スクリプト。

```bash
tools/qemu/bsd-vm.sh <os> <arch> <command> [args]
#   os   : freebsd | openbsd | netbsd
#   arch : x86_64 | aarch64
```

### いちばん短い再現手順

```bash
# setup → provision → toolchain → build → e2e → fetch を一括
tools/qemu/bsd-vm.sh freebsd x86_64 all

# 取得したバイナリを tar.gz 化
./packaging/scripts/build-bsd.sh --os freebsd --arch x86_64 --from-qemu
```

6 通り（freebsd/openbsd/netbsd × x86_64/aarch64）すべて同じ形で実行できる。
取得済みのものをまとめてパッケージ化するなら `build-bsd.sh --all`。

**NetBSD aarch64 は当初 install ISO から `sysinst` をシリアル自動操作していたが、
実機（Apple Silicon + QEMU/HVF）で言語選択メニューのまま止まり動作しないことが
判明したため、x86_64 と同じ「起動可能な生イメージ」経路へ切り替えた**（詳細は
本ドキュメント末尾「NetBSD で踏んだ落とし穴」節を参照）。

### native モード（Docker 不使用、`VEIL_QEMU_NATIVE=1`）

Docker が使えないホスト（**Apple Silicon macOS**（M1〜M4）で Docker 未導入の場合が
主な想定）向けに、helper コンテナを介さず**ホストの qemu-system-\* を直接起動**する
モードを用意している。`VEIL_QEMU_NATIVE=1` を明示するか、`docker` コマンドが
見つからない環境では**自動的に**このモードへ切り替わる（`VEIL_QEMU_NATIVE=0` を
明示すれば docker が無くても自動切替しない）。**Docker が使えるホストでの挙動・
出力は本モードの有無に関わらず一切変更していない**（byte-for-byte 同一）。

macOS（Apple Silicon）での前提:

```bash
brew install qemu       # qemu-system-{aarch64,x86_64} + EDK2 ファーム一式
brew install cdrtools   # mkisofs（cloud-init シード ISO 9660 の作成に使用）
python3 -m pip install --user --break-system-packages pexpect   # provision/autoinstall 系スクリプトが使用
```

**aarch64 ゲスト（FreeBSD/OpenBSD/NetBSD の arm64）は Apple Silicon ホストでは
HVF アクセラレータ**（`-machine virt,accel=hvf,gic-version=3 -cpu host`）で起動する
ため、x86_64 ホストの TCG（数分〜数十分がかりのブート）と違い**ネイティブに近い
速度**で動く。Linux aarch64 の full-system QEMU が KVM 非対応ホストでは TCG で
実用不能だった制約（本 README 下部「既知の環境制約」参照）を、**BSD 系 aarch64 に
限っては Apple Silicon 実機で回避できる**——というのが native モード導入の主眼。

native モードでの相違点（利用者から見て変わるのは主に「Docker を使わない」点のみ、
サブコマンド・引数体系は共通）:

| 項目 | Docker モード | native モード |
|---|---|---|
| qemu 起動 | helper コンテナ内で `qemu-system-*` | ホストの `qemu-system-*`（PATH 上）を `nohup` + `disown` でバックグラウンド起動、pid は `${WORKDIR}/qemu.pid` に記録 |
| cloud-init シード | `cloud-localds` | `mkisofs`/`genisoimage`/`xorrisofs`（無ければ macOS 標準 `hdiutil makehybrid`）で ISO9660(`cidata`) を自作 |
| UEFI ファーム | コンテナ内固定パス（AAVMF/OVMF） | Homebrew（`/opt/homebrew/share/qemu/edk2-*.fd`）等をホスト探索。見つからなければエラー終了 |
| OpenBSD autoinstall の応答ファイル配布 | helper コンテナ内で `python3 -m http.server` | ホスト上で直接 `python3 -m http.server`（`openbsd-autoinstall.py --container` を省略） |
| `status`/`down` | `docker ps`/`docker rm -f` | `qemu.pid` の生死確認 / QMP ACPI シャットダウン→タイムアウトで `kill -9` |
| ポートバインド | `docker run -p` で個別マッピング | qemu プロセス自身が `-serial telnet:0.0.0.0:...` 等で直接バインド（追加の `-p` 相当は不要） |

> **所要時間の目安**（4 コア / KVM 有効ホスト）
> x86_64 ゲストは KVM で加速されるため実用的（FreeBSD amd64 の
> `--features full-freebsd` リリースビルドで実測 **約 30 分**）。
> aarch64 ゲストは x86_64 ホストでは TCG なので数倍〜十数倍かかる。

### サブコマンド

| コマンド | 内容 |
|---|---|
| `all` | 下記を setup → fetch まで一括実行 |
| `setup` | helper イメージ build + ゲストイメージ取得 + SSH 鍵生成（+ FreeBSD は cloud-init シード、OpenBSD は autoinstall 応答ファイル） |
| `reset` | 起動用オーバーレイを作り直して初期状態へ戻す（**再ダウンロード不要**、FreeBSD） |
| `up` / `down` / `status` / `console` | VM ライフサイクル（`down` は ACPI シャットダウンを先に試す） |
| `wait` | SSH 到達までブロック |
| `provision` | SSH 鍵注入まで（OS ごとに方式が違う。下記） |
| `toolchain` | VM 内へ rust / cmake / llvm / gmake 等を導入 |
| `sync` | リポジトリを VM へ転送（tar over ssh） |
| `build` | VM 内でリリースビルド（既定 `--no-default-features --features full-freebsd\|full-openbsd`） |
| `e2e` | VM 内で `tests/e2e_setup.sh test` |
| `fetch` | VM 内の release バイナリを `packaging/build/veil-<os>-<arch>` へ取得 |

> **`sync` / `build` / `e2e` / `fetch` は VM が起動済みであることが前提。**
> 停止中に実行すると内部の `cmd_wait` が SSH 到達を **900 秒** 待ち続け、その間
> **何も出力しないまま**タイムアウトして終了する（「無言でハングした」ように見える）。
> 先に `up`（必要なら `wait`）を実行すること。起動しているかは `status`、
> あるいは `docker ps` に `veil-<os>-<arch>` が出るかで確認できる。
>
> ```bash
> tools/qemu/bsd-vm.sh openbsd x86_64 up
> tools/qemu/bsd-vm.sh openbsd x86_64 wait 900
> tools/qemu/bsd-vm.sh openbsd x86_64 e2e
> ```

### provision の方式（OS で異なる）

| OS | 方式 |
|---|---|
| FreeBSD | 配布の **BASIC-CLOUDINIT** イメージ + NoCloud シード。cloud-init が root パスワード設定と growfs を行い、**SSH 公開鍵はシリアルの getty へ root ログインして注入**する（`freebsd-provision.py --mode login`）。FreeBSD の cloud-init は `write_files` / `runcmd` を実行しないため鍵は cloud-init に任せられない |
| OpenBSD | 配布 VM イメージが無いので **`miniroot<NN>.img` から autoinstall(8)** で無人インストールする（`openbsd-autoinstall.py`）。応答ファイルは helper コンテナ内の HTTP サーバから `http://10.0.2.2:8000/auto_install.conf` として配る。sets は HTTP ミラーから取得。鍵と sshd 設定は autoinstall が行う |
| NetBSD（F-140） | x86_64/aarch64 とも起動可能な**生イメージ**をそのまま使う（x86_64 は `-live.img.gz`、aarch64 は `evbarm-aarch64/binary/gzimg/arm64.img.gz`。amd64 の live image に相当する aarch64 向けブータブルイメージ）。cloud-init 相当が無いため、FreeBSD と同様に**シリアルへ root ログインして鍵を注入**する（`netbsd-provision.py`、両アーキ共通）。旧来 aarch64 は install ISO から `sysinst` をシリアル自動操作していたが、実機で言語選択メニューのまま止まり動作しなかったため廃止した。**x86_64 だけはルートディスクを拡張しない**（B-82: MBR + BIOS 経路のため仮想サイズを変えると CHS ジオメトリが変わり、起動時の fsck が `UNEXPECTED INCONSISTENCY` で失敗する）。代わりに `setup` が `scratch.qcow2` を作って `up` が 2 台目（ゲストの `ld1`）として繋ぎ、`provision` が `newfs` して `/work` にマウントしたうえで `/usr/pkg`・`/var/db/pkgin`・`GUEST_ROOT`・`CARGO_HOME`・`TMPDIR` をそこへ逃がす（ルート FS は 1.8G しかなく空きは ~340M しかないため） |

### ポート割り当て

同時に 4 VM を起動しても衝突しないよう os/arch ごとに固定している
（`SSH_PORT` / `CON_PORT` / `QMP_PORT` で上書き可）。

| VM | ssh | console(telnet) | QMP |
|---|---|---|---|
| freebsd x86_64 | 2310 | 2311 | 2312 |
| freebsd aarch64 | 2320 | 2321 | 2322 |
| openbsd x86_64 | 2330 | 2331 | 2332 |
| openbsd aarch64 | 2340 | 2341 | 2342 |
| netbsd x86_64 | 2350 | 2351 | 2352 |
| netbsd aarch64 | 2360 | 2361 | 2362 |

### 主な環境変数

| 変数 | 既定 | 意味 |
|---|---|---|
| `VEIL_QEMU_NATIVE` | 未設定（`docker` があれば 0 相当、無ければ自動で 1 相当） | 1 で Docker を使わずホストの `qemu-system-*` を直接起動する native モード（上記「native モード」節参照）。0 を明示すると `docker` が無くても自動切替しない |
| `VEIL_QEMU_DIR` | `~/qemu-images` | VM 資材の親ディレクトリ |
| `FREEBSD_VER` / `OPENBSD_VER` / `NETBSD_VER` | `14.3-RELEASE` / `7.9` / `10.1` | ゲスト OS バージョン（OpenBSD の CDN は直近数リリースのみ保持） |
| `NETBSD_PKG_VER` | `10.0` | NetBSD の pkgsrc バイナリパッケージのバージョン系列（OS バージョンとは別軸。`cdn.NetBSD.org` は `.../10.0/All/` を実際のクォータリー版（例 `10.0_2026Q2`）へリダイレクトする） |
| `GROW_GB` | 24 | 起動ディスクに上乗せするサイズ |
| `VM_SMP` / `VM_MEM_MB` | 4 / 4096 | vCPU / メモリ |
| `VM_ROOT_PASSWORD` | `veil` | ゲスト root パスワード（コンソールデバッグ用。SSH は鍵のみ、ポートは 127.0.0.1 のみ） |
| `CARGO_FEATURES` | `full-freebsd` / `full-openbsd` | ビルドする feature セット |
| `BASE_IMG` | — | 既にプロビジョニング済みのイメージを backing file にして起動する（元イメージは変更しない） |
| `VM_FIRMWARE` | `bios` | x86_64 ゲストのファームウェア。`uefi` にすると OVMF + q35 で起動する（`BASE_IMG` に配布の素の VM-IMAGE 由来イメージを指す場合はこちらが必要） |

### ファイル一覧

| ファイル | 役割 |
|---|---|
| `bsd-vm.sh` | **FreeBSD/OpenBSD/NetBSD × x86_64/aarch64 の統合ヘルパ**（本節） |
| `freebsd-provision.py` | FreeBSD の provision。`--mode login`（getty へ root ログインして鍵注入・**現行の既定経路**）/ `--mode ssh`（ローダメニュー経由 single-user）/ `--mode grow`（growfs） |
| `openbsd-autoinstall.py` | OpenBSD の autoinstall(8) をシリアルコンソールから駆動 |
| `netbsd-provision.py` | NetBSD（x86_64/aarch64 とも起動可能な生イメージ）のシリアルログイン provision（SSH 鍵注入・sshd 有効化）。両アーキ共通（F-140） |
| `console-dump.py` | シリアルコンソール（telnet）を非対話で読み出す（`console` サブコマンド） |
| `serial-exec.py` | シリアルへ root ログインして**任意のコマンドを実行**する。SSH が上がらない／壊れた VM の切り分けと復旧に使う（例: unclean な UFS の `fsck` + `mount -u -w /`）。`--con-port` は `SSH_PORT+1` |
| `qmp-sendkeys.py` | QMP 経由の `--key`/`--type`（ブラインド入力）・`--screendump`（ゲスト画面を PNG 化）・`--powerdown`。シリアルに何も出ない状況の切り分けに使う。QMP ポートは `SSH_PORT+2`。例: `python3 tools/qemu/qmp-sendkeys.py --port 2312 --screendump /w/screen.png`（`/w` = ホストの `${WORKDIR}`） |
| `helper/Dockerfile` | qemu-system-{arm,x86} + AAVMF/OVMF + ssh + python3-pexpect + cloud-image-utils |
| `aarch64-vm.sh` / `run-e2e-aarch64.sh` / `linux-aarch64-e2e.sh` | Linux aarch64 用（後述） |
| `fbsd-arm64-vm.sh` / `fbsd-arm64-smoke.sh` | FreeBSD arm64 の従来経路（smoke 専用。新規用途は `bsd-vm.sh` を推奨） |
| `fbsd-capmode-e2e.sh` | capsicum capability mode 静的配信 E2E（F-123） |

### 検証状況

| 項目 | 状況 |
|---|---|
| ゲストイメージ URL（FreeBSD amd64/arm64・OpenBSD 7.9 amd64/arm64 miniroot） | **HTTP 200 を確認済み** |
| FreeBSD x86_64: `setup` → `provision` | **成功**（SSH 鍵認証で `FreeBSD 14.3-RELEASE-p16` へ到達） |
| FreeBSD x86_64: `toolchain` | **成功**（cargo 1.96.1 / cmake 3.31.12 / GNU Make 4.4.1） |
| FreeBSD x86_64: `build`（`full-freebsd`） | **成功**（29分42秒。http3(quiche+共有 aws-lc-sys) / jemalloc / **aio** を含む） |
| FreeBSD x86_64: `fetch` → `build-bsd.sh` | **成功**（`veil-0.6.0-x86_64-unknown-freebsd.tar.gz` を生成） |
| FreeBSD x86_64: `e2e` | **532 passed / 1 failed**（B-50 修正後。HTTP/3 の 117 件失敗はすべて解消）。残り 1 件は実行ごとに変わる負荷起因フレーク（`oversized-header` 等）で、静かなホストでの単体実行では通る |
| （E2E 全般） | `tests/e2e_setup.sh` の `run_tests` が cargo の終了コードを握り潰していた（最後の `log_info` の戻り値が返っていた）。上記 2 件はどちらも «成功» と報告されていた。修正済み |
| FreeBSD aarch64: `build`（`full-freebsd`）→ `e2e`（2026-08-06 実機） | **フルビルド + E2E 実行可能**。540 passed / 2 failed（542 件）。残り 2 件のうち `test_error_handling_oversized_header` は単独実行で pass（既知フレーキー）、`test_http3_large_request_body` は単独実行でも 60 秒タイムアウトで fail（B-61、未解決） |
| OpenBSD x86_64: `setup` → `provision`（autoinstall） | **成功**（`CONGRATULATIONS` → SSH 鍵認証で `OpenBSD 7.9 GENERIC.MP#449 amd64` へ到達） |
| OpenBSD x86_64: `toolchain` | **成功**（cargo 1.94.1 / cmake 4.2.3 / GNU Make 4.4.1 / libprotoc 34.1 / llvm-19） |
| OpenBSD x86_64: `build`（`full-openbsd`） | **成功**（71分56秒。ring + システムアロケータ + quiche/BoringSSL の http3 を含む） |
| OpenBSD x86_64: `fetch` → `build-bsd.sh` | **成功**（`veil-0.6.0-x86_64-unknown-openbsd.tar.gz` を生成） |
| OpenBSD x86_64: `e2e` | **530 passed / 3 failed・SIGSEGV なし**（B-51 / B-52 / B-53 / B-54 修正後。全 533 件が実行される）。残り 3 件はいずれもタイムアウト系の負荷起因フレークで、単体実行では 3 件とも通る |
| OpenBSD x86_64: `e2e`（v0.6.0 最終） | **535 passed / 6 failed / 1 ignored**（F-132〜F-141 + B-56/B-57/B-58 反映後、全 542 件）。**失敗 6 件は単独実行で全て成功**（http3_cache 系 4 件 0.28s / buffering_spillover 0.10s / h2c_large_request_body 0.04s / oversized_request_line 0.24s）＝負荷起因フレークで機能欠陥なし。ignored 1 件は B-58（Pulley で WAF が QUIC idle timeout を超過）。**HTTP/3 + WASM 4 件は全て成功**（B-58 を ignore 化するまでは WAF が HTTP/3 ワーカーを占有して巻き添えにしていた） |
| OpenBSD aarch64: `build`（`full-openbsd`、wasm 込み） | **成功**（B-59 の `CFLAGS_aarch64_unknown_openbsd='-DOPENSSL_STATIC_ARMCAP -DOPENSSL_STATIC_ARMCAP_NEON'` が必要） |
| OpenBSD aarch64: `e2e` | **`test result: ok. 541 passed; 0 failed; 1 ignored`（54.34s）**。wasm 関連テスト 36 件を含め全て pass |
| NetBSD x86_64（F-140、2026-08-06 実機） | **フルビルド + E2E 実行可能**（`full-netbsd`、wasm 込み、18分32秒）。E2E は B-60 の `paxctl +m` 適用後 **530 passed / 12 failed**（適用前は 507 passed / 35 failed で 35 件は全て wasm テスト）。下記「NetBSD で踏んだ落とし穴」参照 |
| NetBSD x86_64（2026-08-29 再測、Apple Silicon 上の **TCG**） | **534 passed / 10 failed**（183.93s。HVF の aarch64 が 43s なので 4 倍以上遅い）。**失敗 10 件を 1 件ずつ再実行して切り分けた**: **5 件は単独なら成功**（`http3_request_body_streaming_tls_backend` 6.14s / `http3_sni_and_cert_reload` 4.44s / `http3_throughput` 0.46s / `http3_udp_unreachable_fallback` 3.26s / `rate_limiting_with_config` 3.85s）＝並列実行時の負荷起因。**残り 5 件（HTTP/3 + WASM 4 件・HTTP/3 + WebSocket 1 件）は単独でも失敗する**。原因は E2E ハーネスのログに出る `Proxy failed to become ready within 180s (WASM AOT compile?)` で、**TCG 上では WASM の AOT コンパイル（Pulley）が 180 秒の起動待ちを超過する**ため。B-58（OpenBSD の Pulley が極端に遅い）と同じ現象がエミュレーションで増幅されたもので、**機能不全ではない**。実機の NetBSD x86_64 では 2026-08-06 に完走している。**「NetBSD x86_64 の失敗は全て負荷起因」という従来の記述は誤り**だった |
| NetBSD aarch64（F-140、2026-08-06 実機） | **フルビルド可**（`full-netbsd`、wasm 込み。B-59 の `CFLAGS_aarch64_unknown_netbsd` 指定が必須）。E2E は **フルスイート完走不能**（B-62: dev-dependency の quinn-udp が NetBSD/aarch64 で panic → プロセスabort）が、`TEST_FILTER=wasm_tests` では `test result: ok. 23 passed; 0 failed; 519 filtered out` |
| **2026-08-29 一斉検証（6 環境すべて）** | 下表参照。**aarch64 は HVF、x86_64 は TCG** |
| `linux-aarch64-e2e.sh` | **未実行**（KVM 非対応ホストでは TCG が実用不能） |

### 2026-08-29: BSD 6 環境の E2E 一斉実行（Apple Silicon / aarch64=HVF・x86_64=TCG）

| 環境 | 結果 | 内訳 |
|---|---|---|
| OpenBSD aarch64 | **543 passed / 0 failed** | 初回は `test_h2c_invalid_frame` が 1 件失敗したが**再実行で解消**（負荷フレーク） |
| NetBSD aarch64 | **421 passed / 0 failed** | **B-62 を修正**して初めて完走できるようになった（従来は SIGABRT で 1 件も走らず）。HTTP/3 はこの環境ではコンパイル対象外 |
| FreeBSD aarch64 | 543 passed / **1 failed** | `test_http3_large_request_body`（**B-68**）。単独実行でも約 50% 失敗する |
| NetBSD x86_64 | 534 passed / **10 failed** | 5 件は単独なら成功（負荷起因）。5 件（HTTP/3+WASM 4・HTTP/3+WebSocket 1）は **TCG 上の WASM AOT が 180 秒の起動待ちを超過**するため |
| OpenBSD x86_64 | 538 passed / **5 failed** / 1 ignored | 全て concurrent/stress 4 件 + rate limiting 1 件。**HTTP/3 の失敗は無し** |
| FreeBSD x86_64 | 535 passed / **9 failed** | 全て concurrent/stress 5 件 + config_validation 2 件 + oversized_header + rate limiting。**HTTP/3 の失敗は無し**（B-68 の `large_request_body` も通過） |

**x86_64（TCG）では単独実行による切り分けができない。** `tests/e2e_setup.sh start` の
サーバ起動（WASM AOT コンパイル）が 180 秒の待ち時間を超過し、さらに feature/env が
少しでも違うとフルリビルド（実測 41 分）が走るため。切り分けが必要なら
**フルスイートを回した直後**の、サーバが温まった状態で行うこと
（NetBSD x86_64 の 10 件はこの方法で切り分けた）。

### FreeBSD amd64 で踏んだ落とし穴（すべて実測。再発しやすいので残す）

1. **配布されている素の `FreeBSD-<ver>-RELEASE-amd64.qcow2` は使えない。**
   シリアルへ一切出力しないうえ、ローダが `Loading configured modules...` を出した
   あとカーネルが起動せずハングする（`-machine q35`/`pc`、`-cpu host`/`qemu64`、
   BIOS/UEFI の全組み合わせで再現）。
   → **`BASIC-CLOUDINIT` 版**を使う。こちらは `Dual Console: Serial Primary` で
   シリアルが有効、かつ正常に起動する。

2. **配布イメージの仮想サイズは ~6GiB しかない。**
   初回ブートの `freebsd-update` がデバッグシンボルを展開してゲストの FS が満杯になり、
   `No space left on device` を延々と出して sshd まで到達しない。
   → `setup` は配布イメージを `base.qcow2` として保持し、起動用の
   **qcow2 オーバーレイ**を `+${GROW_GB}G` して作る。cloud-init の growfs が拡張する。
   `reset` でオーバーレイだけ作り直せる（再ダウンロード不要）。

3. **FreeBSD の cloud-init は `chpasswd` は適用するが `write_files` / `runcmd` は
   実行しない。** → cloud-init には root パスワード設定と growfs だけを任せ、
   **鍵注入はシリアルの getty へ root ログインして行う**。

4. **cloud-init シードは CD-ROM として繋ぐ。**
   virtio-blk のディスクとして足すと、ローダが起動デバイスを取り違えて
   `Failed to load kernel 'kernel'` でローダプロンプトに落ちる。
   ルートディスクは `-drive if=virtio,index=0` のままにすること
   （`-drive if=none` + `-device virtio-blk-pci,bootindex=0` に変えると
   boot2 が `Booting from Hard Disk...` のスピナーのまま進まなくなる）。

5. **`down` は ACPI シャットダウンを先に試す。**
   cloud-init / growfs の書き込み中に `docker rm -f`（= qemu へ SIGKILL）すると
   イメージが壊れ、次回の boot2 が回り続ける。

6. **`-serial ...,server,nowait` は接続前の出力を捨てる。**
   ローダメニューなど起動直後の出力を見たいときは、VM 起動直後に接続すること。

7. **`tikv-jemalloc-sys` は `gmake` を要求する。**
   `full-freebsd` は jemalloc を含むため、`toolchain` で `gmake` を入れないと
   `failed to execute command: No such file or directory` で落ちる。

8. **`scp` のポート指定は `-P`**（`-p` は「タイムスタンプ保持」）。
   ssh 用のオプション配列をそのまま流用すると 22 番へ繋ぎに行って `fetch` が失敗する。

9. **電源断（`docker rm -f` / ホスト再起動）の後始末は 3 箇所ある。**
   UFS が unclean のまま起動すると `/` が **read-only でマウント**され、rc の
   ネットワーク設定が適用されないまま sshd だけが上がる。この状態は外からは
   `Connection timed out during banner exchange` にしか見えず原因が分かりにくい。
   `serial-exec.py` でシリアルから入って復旧する:

   ```bash
   python3 tools/qemu/serial-exec.py --con-port 2311 --timeout 1200 \
     'fsck -y /dev/gpt/rootfs' 'mount -u -w /' 'sync; reboot'
   ```

   同じ電源断で次の 2 つも壊れることがある（どちらも実測）:
   - `pkg` の `/var/db/pkg/local.sqlite` → `database disk image is malformed` /
     `Assertion failed: (p != NULL) ... pkg_jobs_conflicts.c`。
     `mv /var/db/pkg/local.sqlite /var/db/pkg/local.sqlite.corrupt` してから
     `toolchain` をやり直す（実体のファイルは残っているので上書きインストールされる）。
   - cargo のレジストリキャッシュ → `failed to unpack package ...` /
     `numeric field was not a number`。`rm -rf $CARGO_HOME/registry` で解消する。

10. **`BASE_IMG` に配布の素の VM-IMAGE 由来イメージを指す場合は UEFI 起動が要る。**
    既定の SeaBIOS（`-machine pc`）ではカーネルまで進まず画面が真っ黒のまま止まる。
    `VM_FIRMWARE=uefi` を指定すると OVMF + q35 で起動する（この経路ではカーネルの
    出力は `Dual Console: Serial Primary` 以降シリアルへ移る）。

### OpenBSD で踏んだ落とし穴（すべて実測）

quiche が使う **BoringSSL（boring-sys）は OpenBSD を想定していない**ため、
ビルド環境側での回避が複数必要になる。以下はすべて `bsd-vm.sh` が自動で行う。

1. **`/` が ~628M しかない。** autoinstall の auto layout は `/` を小さく取り、
   `/usr/obj` が最大（24G ディスクで ~8G）。`GUEST_ROOT` と `CARGO_HOME` を
   `/usr/obj` 配下にしないと `No space left on device` になる。

2. **`llvm` が黙って入らない。** OpenBSD には llvm-19/20/21 が並存するため、
   曖昧な `llvm` を `pkg_add -I`（非対話）へ渡すと失敗する。その結果
   `LIBCLANG_PATH` が空になり bindgen が
   `Unable to find libclang ... (invalid: [])` で落ちる。
   `pkg_info -Q llvm` から具体的なバージョンを選んで入れる。

3. **`pthread_rwlock_t` が見つからない。** BoringSSL の `openssl/thread.h` は
   `pthread_rwlock_t` が `<sys/types.h>` から見える前提だが（glibc/FreeBSD/macOS
   では真）、OpenBSD では `<pthread.h>` にしかない。
   → **C ファイルのときだけ** `-include pthread.h` する cc ラッパ
   （`/usr/local/bin/veil-cc`）を `CC_<target>` として使う。
   `CFLAGS_<target>` に足すと **アセンブリ(.S) にも**適用され、zstd-sys の
   `huf_decompress_amd64.S` が `unknown token in expression` で壊れる。

4. **bindgen にも同じ指定が要る。** bindgen は cc ラッパを経由せず自前の clang で
   ヘッダを解析するため、`BINDGEN_EXTRA_CLANG_ARGS_<target>` にも
   `-include pthread.h` を渡す。

5. **`-lstdc++` が無い。** boring-sys は非 Apple ターゲットで `-lstdc++` を要求するが、
   OpenBSD の C++ 標準ライブラリは **libc++**。
   `/usr/local/lib/libstdc++.so → /usr/lib/libc++.so.N` の互換リンクを張り、
   `RUSTFLAGS='-L /usr/local/lib'` で解決する。

6. **cmake はコンパイラ設定をキャッシュする。** 上記 3 の対処を入れても、
   前回失敗時の `target/*/build/boring-sys-*/out/build` が残っていると
   `CMakeCache.txt` の古い `CMAKE_C_COMPILER` が使われ続ける。
   環境変数を変えたときは boring-sys の build ディレクトリを消してから再実行する。

7. **`/` 以外は `wxallowed` ではない。** OpenBSD の既定 fstab で `wxallowed` が付くのは
   **`/usr/local` だけ**。ネイティブ JIT（wasmtime の Cranelift 等）は
   `mprotect(PROT_EXEC)` に実行ファイルが `wxallowed` マウント上にあることを要求するため、
   `/usr/obj` にビルドした veil では JIT が使えない。
   veil は OpenBSD では **Pulley インタープリタ**を使うのでこの制約を受けないが、
   切り分けで JIT を試すときは `mount -u -o wxallowed /usr/obj` が要る（B-52）。

8. **`/usr/obj` が ~8G しかないためデバッグ情報付きビルドは入らない。**
   `target/debug` が 5G 超 + `incremental` 1.5G で `No space left on device` になる。
   `bsd-vm.sh` はゲスト側 env に `CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0` を
   付けて回避する（E2E はデバッガを使わないため実害なし。target が ~2.3G に収まる）。

9. **`-serial ...,server,nowait` はブートローダのプロンプトを取り逃す。**
   OpenBSD amd64 は `boot>` へ `set tty com0` を送ってシリアルへ切り替える必要が
   あるが、`nowait` だと接続前に流れてしまう。`provision` は `CONSOLE_WAIT=1` で
   **qemu にコンソール接続を待たせて**から起動する。

> aarch64（arm64）はこれらのうち 1・3・4 の影響を受けない。UEFI + efiboot が
> ファームウェアの ConOut を引き継ぐためシリアルが既定で使え、素直に起動する。

### NetBSD で踏んだ落とし穴（F-140、**実 VM で実測済み**）

**2026-07-29 に実 VM（NetBSD 10.1 amd64 / QEMU）で確認した結果:**

1. **x86_64 の live image はそのままブートでき、autoinstall は不要**（想定どおり）。
   `-live.img.gz` を `qemu-img convert` で qcow2 化するだけで起動する。
2. **カーネルコンソールは既定で VGA（`ttyE0`）。シリアルに getty が出ない。**
   ブートローダのメニューはシリアルにも出るが、カーネル起動後の出力は VGA だけになる。
   ブートメニューで `3`（Drop to boot prompt）→ `consdev com0` → `boot` とすると
   シリアルがカーネルコンソールになり、`/etc/ttys` の `console` エントリが追従して
   シリアルにログインプロンプトが出る。初回ログイン後に `/boot.cfg` へ
   `consdev=com0` を書き込めば以降は自動（`netbsd-provision.py` が実施）。
3. **ブートメニューのカウントダウンは 5 秒しかない。**
   `-serial ...,server,nowait` + 固定 `sleep` では qemu/docker の起動オーバーヘッドで
   カウントダウンを消費してしまい、メニューを一度も観測できずに VGA へ自動ブートする。
   **`CONSOLE_WAIT=1`（`server,wait`）** でスクリプト接続まで qemu を待たせること。
4. **メニューの `Option: [1]:` プロンプトは単一キーではなく行入力。**
   `send("3")` ではなく `sendline("3")` が必要（実測）。
5. **SSH 非対話シェルの既定 PATH に `/usr/sbin` が無い**
   （実測: `PATH=/usr/bin:/bin:/usr/pkg/bin:/usr/local/bin`）。
   `pkg_add(8)` は `/usr/sbin` にあるため**絶対パスで呼ぶ**必要がある。
   `pkgin` も `/usr/pkg/bin` なので同様。
6. **`uname -m` は `amd64` を返すが pkgsrc のパス要素は `x86_64`**
   （`amd64` は `x86_64` へ 302 リダイレクト）。また **NetBSD のリリース番号（10.1）と
   pkgsrc パッケージのディレクトリ番号（10.0）は異なる**。
7. **Rust は `rust-bin`（バイナリパッケージ）を使うこと。** ソースの `rust` は
   QEMU 上で数時間かかる。
8. **NetBSD aarch64 の install ISO + `sysinst` シリアル自動操作は実機で動作しなかった。**
   Apple Silicon + QEMU/HVF で実際に検証したところ、
   sysinst の**言語選択メニューで停止**し、以降の自動化が一切進まなかった
   （メニュー文言・キー割り当ての推定が実機と食い違っていたと見られる）。
   NetBSD は amd64 の live image と同様に、`evbarm-aarch64` 向けの**起動可能な
   生イメージ**（`gzimg/arm64.img.gz`）も配布していることを確認できたため、
   sysinst 自動操作は全面的に廃止し、x86_64 と全く同じ「生イメージ →
   `base.qcow2` 化 → オーバーレイ起動 → シリアルログイン provision
   （`netbsd-provision.py`）」経路に一本化した。sysinst 自動操作用のスクリプトは
   削除した。
9. **wasmtime 40 は NetBSD を全アーキテクチャでサポートしない（B-55 更新）。**
   `cargo build --no-default-features --features full-netbsd`（当時 `wasm` 込み）が
   依存の wasmtime でコンパイルエラーになった:
   ```
   error: unsupported platform
     --> wasmtime-40.0.4/src/runtime/vm/sys/unix/signals.rs:329:13
         compile_error!("unsupported platform");
   error: unsupported platform
     --> wasmtime-40.0.4/src/runtime/vm/sys/unix/signals.rs:401:13
   error: could not compile `wasmtime` (lib) due to 2 previous errors
   ```
   wasmtime のシグナルベーストラップ実装（`signals.rs`）には NetBSD 向けの
   `ucontext` 分岐が一切無く、**x86_64 ですら**このエラーになる。B-55 は当初
   「BSD × aarch64 のみ非対応」という内容だったが、NetBSD は x86_64 を含む全
   アーキテクチャで非対応であることが判明したため範囲を拡張した
   （`has_native_signals` はホストの target_arch から wasmtime の build.rs が
   導出するため cargo feature で無効化できず、Pulley インタープリタへの切替でも
   回避できない）。対応として `Cargo.toml` の `full-netbsd` からも `wasm` を除外した
   （`full-netbsd` は元々除外済み）。詳細は
   `docs/backlog/bugs/B-55-wasmtime-no-bsd-aarch64.md` 参照。

以下は設計時点の想定として残す（上記で否定/確認された項目を含む）。

NetBSD は 2026-07-28 時点で入手性のみ実地確認済み（ゲストイメージ URL・pkgsrc
バイナリパッケージ URL とも `curl -sIL` で HTTP 200 を確認済み）。**実際の QEMU
起動・provision・toolchain・build・e2e はコーディネーターが別途行う**ため、以下は
FreeBSD/OpenBSD の実測知見から類推した設計上の想定であり、実機での検証で変わる
可能性が高い。

1. **x86_64 は live image を使うことで OpenBSD のような autoinstall が不要という
   想定。** `-live.img.gz` は起動可能な生イメージ（rootfs 込み）なので、
   `qemu-img convert` で qcow2 化するだけでブート可能なはず。ただし cloud-init
   相当が無いため、SSH 鍵注入は FreeBSD の `--mode login` と同じ発想で
   **シリアルへ root ログインして行う**（`netbsd-provision.py`）。
   - NetBSD の live image が **root パスワード空**でログインできるか、
     ネットワークが起動時に自動設定（dhcpcd 等）されるかは未検証。
   - x86 のブートローダ（boot.cfg）が既定でシリアルへ出力するか、VGA
     コンソールへ出力しメニュー操作でシリアルへ切り替える必要があるかは
     FreeBSD/OpenBSD 同様に不透明（`netbsd-provision.py` はどちらにも
     対応しようとするベストエフォート実装）。
2. ~~aarch64 は install ISO のみで、`sysinst` の自動操作が要る。~~
   **この想定は誤りだった（上記「NetBSD で踏んだ落とし穴」項目 8 参照）。**
   `sysinst` のシリアル自動操作は実機で言語選択メニューのまま止まり動作しなかった。
   aarch64 にも amd64 の live image に相当する**起動可能な生イメージ**
   （`evbarm-aarch64/binary/gzimg/arm64.img.gz`）が配布されていたため、
   install ISO + sysinst 経路は全面的に廃止し、x86_64 と同じ経路へ一本化した。
3. **toolchain は `rust-bin`（バイナリ）を使うことが必須。**
   pkgsrc には `rust`（ソースビルド）と `rust-bin`（プリビルド）が並存する。
   `rust` を選ぶと OpenBSD のソースビルドと同様に QEMU 上で数時間かかるため、
   `cmd_toolchain` は明示的に `rust-bin` を指定する。`pkgin` 自体が既定で
   入っているかは不明なため、`command -v pkgin` が失敗したら `pkg_add -v pkgin`
   で bootstrap するようにしてある（bootstrap kit の入手方法は他にもあり、
   実機で失敗する場合は要調整）。
4. **pkgsrc のパッケージバージョン系列（`NETBSD_PKG_VER`）は OS バージョンと別軸。**
   `cdn.NetBSD.org/pub/pkgsrc/packages/NetBSD/<arch>/10.0/All/` は実際には
   `10.0_2026Q2/All/` 等へ 302 リダイレクトされる（`curl -fL` で追従される）。
   NetBSD 10.1 の OS イメージに対して pkgsrc は `10.0` 系列を使う点に注意
   （タスク依頼時点の実地調査結果をそのまま採用）。
5. **x86_64 と aarch64 で Rust のバージョンが異なる**（pkgsrc `rust-bin-1.96.0` /
   `rust-bin-1.91.1`）。veil の MSRV を満たすかは実機 toolchain 実行時に
   `cargo --version` で確認すること。
6. **GUEST_ROOT / パーティションサイズは未検証。** OpenBSD の autoinstall auto
   layout のように `/` が極端に小さい構成になるかは、NetBSD の live image /
   sysinst のデフォルトパーティショニング次第。ビルドが `No space left on
   device` で落ちる場合は OpenBSD と同様に `GUEST_ROOT` を広いパーティションへ
   変更する対応が要る（`bsd-vm.sh` の `GUEST_ROOT` 環境変数で上書き可能）。
7. **NetBSD には pledge/unveil 相当が無い。** サンドボックス面は chroot(2) +
   特権降格のみ（`src/security.rs` の `netbsd` モジュール、F-140 コード側で
   実装済み）。VM 内 E2E で capsicum/pledge 相当のテストケースが skip される
   想定になっているか要確認。

## Linux aarch64（io_uring、`aarch64-vm.sh`）

veil の **io_uring バックエンド**を実 aarch64 カーネル上で E2E / `tools/perf` 検証する。

## なぜ full-system emulation か

- QEMU **user-mode**（`qemu-aarch64`）は `io_uring` の syscall
  （`io_uring_setup`/`io_uring_enter` と mmap 経由の SQ/CQ 共有リング）を正しく
  エミュレートできず、独自 io_uring ランタイムが起動できない。
- そのため **full-system emulation**（`qemu-system-aarch64` + 実 Linux カーネル）を
  使い、ゲスト内で本物の io_uring を動かす。
- epoll バックエンド（`--features epoll`）の aarch64 検証は user-mode QEMU でも可能で、
  そちらは `docker/Dockerfile.{glibc,musl}.aarch64` 側でカバーする。ここは
  **io_uring 専用**の検証環境。

## 前提と制約

- ホストに `qemu-system-aarch64` / UEFI ファーム / `cloud-image-utils` が無く sudo も
  使えない環境を想定し、これらを内包した Docker ヘルパイメージ（`helper/Dockerfile`）
  経由で QEMU を起動する。
- x86_64 ホスト上の aarch64 full-system は **TCG（ソフトウェアエミュレーション）** で
  動くため非常に低速。**VM 内でのフルビルドは避け**、ホストでクロスコンパイルした
  バイナリのみを VM へ転送して実行する。
- **既知の環境制約（重要）**: KVM が使えない（クロスアーチ）ホストでは TCG のみとなり、
  汎用クラウドイメージ（Ubuntu cloud image / Alpine cloud image いずれも）は
  systemd/OpenRC の初期化やサービス依存解決の段階で `hrtimer: interrupt took ...` を
  伴い実用不能なほど遅く（`soft lockup CPU#0 stuck` に至る場合もある）、SSH 到達前に
  停滞することがある。この場合、**フルシステムの対話的 E2E は当該ホストでは成立しない**。
  aarch64 の妥当性確認は次の 2 点で代替する:
  1. 現行コードの **aarch64 クロスビルド成功**（`messense/rust-musl-cross:aarch64-musl`
     で `aws-lc-sys` の bindgen に `BINDGEN_EXTRA_CLANG_ARGS=--sysroot=...` を渡す。
     成果物は実 aarch64 ELF・静的リンク）。
  2. io_uring 経路は **アーキテクチャ非依存**（カーネル io_uring ABI は LE 全アーチで
     同一、SQE/CQE の struct レイアウトも共通）であることのコードレベル論証。

  KVM 対応（ネイティブ aarch64 ホスト or ネスト仮想化）が使えるホストでは、上記
  `up`→`wait`→`run-e2e-aarch64.sh` がそのまま実 io_uring E2E として機能する。

## 使い方

```bash
# 1. 環境準備（ヘルパイメージ build + cloud image DL + cloud-init seed）。初回のみ。
tools/qemu/aarch64-vm.sh setup

# 2. VM 起動（detached）
tools/qemu/aarch64-vm.sh up

# 3. SSH 到達まで待機（TCG のため初回は数分〜数十分）
tools/qemu/aarch64-vm.sh wait

# 4. VM 内でコマンド実行 / ファイル転送
tools/qemu/aarch64-vm.sh ssh 'uname -mr; cat /proc/sys/kernel/io_uring_disabled'
tools/qemu/aarch64-vm.sh scp ./veil /home/veil/veil

# 5. 後片付け
tools/qemu/aarch64-vm.sh down
```

## E2E / perf の一括実行

`run-e2e-aarch64.sh` が「ホストで aarch64-gnu クロスビルド → VM へ転送 → io_uring で
起動 → HTTPS スモーク E2E」を一括で行う。`tools/perf` の aarch64/io_uring 実行にも
同じ VM を使う（`PERF_TARGET=aarch64-qemu` は `tools/perf/run_perf.sh` から本 VM の
SSH 経由でバイナリを起動する）。

```bash
tools/qemu/run-e2e-aarch64.sh          # クロスビルド + VM E2E スモーク（HTTPS 静的配信のみ）
```

より強い検証として、**Docker でクロスビルドした aarch64 バイナリを VM へ持ち込み、
`tests/e2e_setup.sh test` の全 E2E スイートを実 aarch64 カーネル上で回す**スクリプトも
用意している（B-47）。

```bash
tools/qemu/linux-aarch64-e2e.sh        # docker build → VM 転送 → tests/e2e_setup.sh test
```

veil 本体は `docker/Dockerfile.glibc.aarch64` の成果物を使い、VM 内では
`VEIL_E2E_SKIP_VEIL_BUILD=1` でビルドを省略する（`grpc_server` / `test_backends` と
E2E テストバイナリは cargo が要るので VM 内でビルドされる）。上記「既知の環境制約」の
とおり、KVM が使えないホストでは TCG が遅すぎて完走しない。

## VM 仕様

- ゲスト: Ubuntu 24.04 arm64 cloud image（`-machine virt -cpu cortex-a72`）。
- ユーザ: `veil` / パスワード `veil`（SSH は公開鍵認証。鍵は `setup` で自動生成）。
- SSH: ホスト `127.0.0.1:2222`（`SSH_PORT` で変更可）。
- 起動高速化のため cloud-init で `snapd`/`multipathd` を mask する。
- 資材は `~/qemu-images/aarch64/`（`VEIL_QEMU_DIR` で変更可）。

---

## FreeBSD arm64（aarch64、`fbsd-arm64-vm.sh`）

**FreeBSD 14.x arm64 VM-IMAGE は Linux aarch64 と異なり TCG（KVM 不可ホスト）でも実用
起動する**。よって aarch64 の「ビルド + 動作確認」を当環境で実施できる（v0.6.0 で確立）。
aarch64-unknown-freebsd は Rust Tier3（prebuilt std 無し・cross は build-std 要）のため、
**VM 内でネイティブビルド**する（`pkg install rust cmake llvm` → `cargo build`）。

```bash
# 1. helper build + VM-IMAGE DL + 鍵生成（初回のみ）
tools/qemu/fbsd-arm64-vm.sh setup
# 2. VM 起動（telnet シリアルコンソール + hostfwd ssh）
tools/qemu/fbsd-arm64-vm.sh up
# 3. ディスクを +20G 拡張（rust + build に必要。root FS 既定 ~5G では不足）
tools/qemu/fbsd-arm64-vm.sh grow
# 4. single-user 経由で root SSH を有効化（鍵注入 + sshd 有効化）
tools/qemu/fbsd-arm64-vm.sh provision
# 5. source 転送 → in-VM ネイティブビルド → HTTPS 静的配信 smoke
tools/qemu/fbsd-arm64-vm.sh smoke        # 期待: ARM_SMOKE=PASS（HTTP 200）
# 6. 後片付け
tools/qemu/fbsd-arm64-vm.sh down
```

### 落とし穴（project memory / v0.6.0 検証で確認済み）

- `virtio-net-pci` には **`romfile=`（空）** が必須（`efi-virtio.rom` 不足で起動失敗）。
- シリアルに **getty が無く root SSH も既定無効** → `provision` は **loader メニューで
  single-user（"2"）** を選び、getty 不要の root シェルから鍵注入 + `sysrc sshd_enable=YES`
  + `PermitRootLogin yes` を行う（`freebsd-provision.py --mode ssh`）。
- qemu の **telnet シリアルコンソールは IAC(0xff)** を送るため pexpect は `encoding="latin-1"`。
  unix socket は root 所有で非 root が connect できないため **TCP telnet** を使う。
- **root FS が ~5G と小さい**。`qemu-img resize` 後の online growfs はマウント中 root で
  "not clean" 拒否 → **single-user で / を `mount -u -o ro /` → `fsck` → `growfs`**
  （`grow` サブコマンドが gpart resize + fsck + growfs を実施）。
- `pkg` は **`IGNORE_OSVERSION=yes ASSUME_ALWAYS_YES=yes`** で userland 版不一致プロンプト回避。
- aws-lc-sys（FreeBSD は aws_lc_rs provider）の bindgen が **libclang** を要求 →
  `pkg install llvm` + `LIBCLANG_PATH=/usr/local/llvmNN/lib`。
- sshd は TCG で **banner 応答が遅い** → ssh `ConnectTimeout=90` 程度。
- **TCG のクリーンビルドは数時間規模**（aws-lc-sys の C ビルドが律速）。

### スクリプト

| ファイル | 役割 |
|---|---|
| `fbsd-arm64-vm.sh` | ライフサイクル（setup/up/grow/provision/smoke/ssh/down）。**新規用途では `bsd-vm.sh freebsd aarch64` を推奨**（本スクリプトは smoke 専用の従来経路として残置） |
| `freebsd-provision.py` | single-user 経由の SSH 鍵注入（`--mode ssh`）/ growfs（`--mode grow`） |
| `fbsd-arm64-smoke.sh` | VM 内 HTTPS 静的配信 smoke（`veil` 起動 → curl → 200 判定） |
| `fbsd-capmode-e2e.sh` | capsicum capability mode 静的配信 E2E（F-123。arch 非依存で amd64/arm64 とも） |

### 環境変数

- `WORKDIR`（既定 `~/qemu-images/fbsd-aarch64`）・`IMG`・`SSH_PORT`(2223)・`CON_PORT`(2224)・
  `KEY`（既定 `~/.ssh/veil_qemu_key`）・`GROW_GB`(20)・`HELPER_IMG`(`veil-qemu-aarch64:local`)。
