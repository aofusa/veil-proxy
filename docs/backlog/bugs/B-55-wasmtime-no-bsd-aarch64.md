# B-55: wasmtime が BSD の一部プラットフォームをサポートしておらず `full-*` の一部がビルドできない

**状態: 対応済み（`third_party/wasmtime` vendoring + ターゲット別依存 + Pulley 強制で該当プラットフォームでも `wasm` を有効化、`feat/bsd-wasm-integration`）**

**当初は「BSD × aarch64」のみの制約だと考えていたが、2026-07-29 の NetBSD 実機検証で
NetBSD は **x86_64 を含む全アーキテクチャ**で wasmtime 非対応であることが判明した
（範囲を拡張）。**

## 事象

### FreeBSD/OpenBSD × aarch64（当初の発見、QEMU VM）

FreeBSD 14.3 arm64（QEMU VM）で `--no-default-features --features full-freebsd`
（= `wasm` を含む）をビルドすると、依存の wasmtime でコンパイルエラーになる。

```
error: unsupported platform
   --> /root/.cargo/registry/src/index.crates.io-.../wasmtime-40.0.4/
       src/runtime/vm/sys/unix/signals.rs:401:13
    |
401 |             compile_error!("unsupported platform");
```

### NetBSD × x86_64（2026-07-29 に実機で追加確認）

NetBSD 10.1 amd64（QEMU 実機）で `cargo build --no-default-features --features
full-netbsd`（当時は `wasm` を含んでいた）を実行したところ、**x86_64 であるにも
関わらず**同じくコンパイルエラーになった。

```
error: unsupported platform
  --> wasmtime-40.0.4/src/runtime/vm/sys/unix/signals.rs:329:13
      compile_error!("unsupported platform");
error: unsupported platform
  --> wasmtime-40.0.4/src/runtime/vm/sys/unix/signals.rs:401:13
error: could not compile `wasmtime` (lib) due to 2 previous errors
```

## 原因

wasmtime のシグナルベーストラップ実装は、トラップ時に `ucontext` を書き換えて
実行を復帰させるため **OS × アーキごとのレジスタ名を直に触る**。その分岐が
`cfg_if` で列挙されており、**BSD は FreeBSD/OpenBSD の x86_64 しか対応しておらず、
NetBSD はアーキ問わず分岐自体が存在しない**:

| 分岐 | 対応 |
|---|---|
| `linux` × `x86_64` / `aarch64` / `s390x` / `riscv64` | ✅ |
| `apple`（macOS/iOS）× `x86_64` / `aarch64` | ✅ |
| **`freebsd` × `x86_64`** | ✅ |
| **`openbsd` × `x86_64`** | ✅ |
| **`freebsd` × `aarch64`** | ❌ `compile_error!` |
| **`openbsd` × `aarch64`** | ❌ `compile_error!` |
| **`netbsd` × `x86_64`** | ❌ `compile_error!`（**x86_64 ですら未対応**） |
| **`netbsd` × `aarch64`** | ❌ `compile_error!` |

さらにこの経路は **cargo feature では無効化できない**。`signals.rs` は
`#[cfg(has_native_signals)]` で囲われており、`has_native_signals` は wasmtime 自身の
`build.rs` が **ホストの `CARGO_CFG_TARGET_ARCH`** から導出する:

```rust
let has_host_compiler_backend = matches!(target_arch, "x86_64" | "riscv64" | "s390x" | "aarch64");
let has_native_signals = !miri && (supported_os || cfg!(feature = "custom-native-signals"))
    && has_host_compiler_backend;
```

`x86_64`/`aarch64` はいずれも `has_host_compiler_backend = true` なので、
FreeBSD/OpenBSD の aarch64 でも NetBSD の x86_64/aarch64 でも `has_native_signals` が
立ち、対応する `cfg_if` 分岐が無いまま `signals.rs` がコンパイルされてしまう。
NetBSD の場合はそもそも `cfg_if` に `netbsd` という OS 分岐自体が存在しない
（FreeBSD/OpenBSD は x86_64 分岐だけは用意されている点でまだ NetBSD よりマシ）。

**Pulley（B-52 で OpenBSD に導入したインタープリタ）でも回避できない。**
Pulley は `Config::target("pulley64")` による**コード生成先**の切り替えであって、
上記 `build.rs` の判定は**ホストの target_arch**（および target_os）を見るため
影響しない。NetBSD で Pulley をターゲットに指定しても、ホスト側のビルドで
`signals.rs` 自体のコンパイルが先に失敗する。

## 影響

- **FreeBSD/OpenBSD × aarch64** では **Proxy-Wasm 拡張が使えない**（x86_64 は影響なし）。
- **NetBSD** は **x86_64 を含む全アーキテクチャ**で **Proxy-Wasm 拡張が使えない**。
- それ以外（HTTP/1.1・HTTP/2・**HTTP/3**・gRPC・WebSocket・L4・圧縮・キャッシュ・
  レート制限・バッファリング・admin・アクセスログ・kTLS(FreeBSD)・AIO(FreeBSD)）は
  上記プラットフォームでも利用できる。

（上記は発見当時の事象。下記「対応（解消）」により現在はこれらのプラットフォームでも
Proxy-Wasm が利用できる。）

## 初期対応（暫定回避、2026-07 時点。下記「対応（解消）」で置き換え済み）

cargo は **feature セットを target 別に切り替えられない**（`[target.*]` は依存関係
専用で features には効かない）ため、既存の `full-freebsd` / `full-openbsd` と同じく
**プラットフォーム別の feature セットを明示指定する**方式にそろえた。

- `Cargo.toml` に `full-freebsd-aarch64` / `full-openbsd-aarch64`（それぞれ
  `full-freebsd` / `full-openbsd` から **`wasm` だけを除いたもの**）を追加。
- `tools/qemu/bsd-vm.sh` の `_default_features()` が `ARCH=aarch64` のとき
  自動で `-aarch64` 付きのセットを選ぶ。
- NetBSD は x86_64/aarch64 の区別なく非対応と判明したため、`full-netbsd` /
  `full-netbsd-vendor` / `full-netbsd-aarch64` / `full-netbsd-aarch64-vendor` の
  **4 つ全てから `wasm` を除外**した（`full-netbsd-aarch64*` は元々除外済みだった
  ため変更不要、`full-netbsd`/`full-netbsd-vendor`（x86_64 向け）から新たに除外）。

これは「対象プラットフォームでは Proxy-Wasm を諦める」という機能除外であり、
根本原因（wasmtime 側の `ucontext` 分岐欠如）そのものの解消ではなかった。

## 対応（解消、`feat/bsd-wasm-integration`）

上記の暫定回避を撤回し、対象 3 ターゲット（NetBSD 全アーキ・FreeBSD aarch64・
OpenBSD aarch64）でも `wasm` を有効なまま使えるようにした。検討した 2 案
（vendoring 案 A・wasmi 別実装案 B）と判断根拠は
`docs/artifacts/b55_bsd_wasm_design.md` 参照（B を不採用にしたのは wasmi 版
ホスト実装のスタブが多くプラットフォーム間で挙動が変わりうるため）。

**やったこと**:

1. **crates.io wasmtime 40.0.4 を `third_party/wasmtime` へ vendoring**し、
   パッケージ名だけ `veil-wasmtime` に変更（`[lib] name = "wasmtime"` は据え置き）。
   差分は `Cargo.toml` の名前変更 + `[lints.rust] dead_code = "allow"` と、
   `build.rs` の `has_native_signals` 算出に対象 3 ターゲットで `false` を強制する
   1 箇所のみ（詳細・由来・追従手順は `third_party/wasmtime/README.veil.md`）。
2. **Cargo のターゲット別依存**（`[target.'cfg(...)'.dependencies]`）で
   `wasmtime`（crates.io 版）と `veil-wasmtime`（path 版）を相互排他に切り替え。
   `[patch.crates-io]` は全ターゲットの wasmtime を差し替えてしまうため使わず、
   依存キーを分けることで cargo の
   `Dependency 'wasmtime' has different source paths depending on the build target`
   エラーを回避した。`[lib] name` が同じ `wasmtime` のままなので `src/` の
   `use wasmtime::...` は 1 行も変更不要。
3. `build.rs` が `veil_wasm_nosignals` cfg（対象 3 ターゲットで true）を発行し、
   `src/wasm/registry.rs` はこの cfg が立っているとき常に Pulley インタープリタを
   強制する（既存の OpenBSD 常時 Pulley 強制と統合）。ネイティブ JIT を生成しない
   ため、そもそもシグナルベース trap が不要という点は当初の分析どおり。
4. `full-freebsd-aarch64` / `full-openbsd-aarch64` / `full-netbsd` /
   `full-netbsd-aarch64` の 4 feature セットすべてに `wasm` を復活させた。

**適用範囲の限定**: Linux / Windows / macOS / FreeBSD x86_64 / OpenBSD x86_64 は
crates.io の wasmtime 40.0.0 をソース・依存とも一切変えずそのまま使う
（vendoring は対象 3 ターゲットのビルド時にのみ選択される）。

**残課題（上流）**: 本対応はあくまで veil 側のローカルな回避策であり、
下記「上流について」に書いた `ucontext` 分岐を wasmtime 本体に追加するのが
本来の直し方であることに変わりはない。wasmtime のバージョンを上げるたびに
`third_party/wasmtime/README.veil.md` の「新しい wasmtime バージョンへ追従する
手順」に従って `has_native_signals` 差分を再適用する必要がある（自動追従の
仕組みは無い）。

## 上流について

FreeBSD/OpenBSD については wasmtime に `(freebsd, aarch64)` / `(openbsd, aarch64)`
の `ucontext` 分岐を足せば解消する見込み。FreeBSD arm64 の `mcontext_t` は
`mc_gpregs.gp_elr` / `gp_sp` / `gp_x[]` を持つため、Linux aarch64 の分岐
（`uc_mcontext.pc` / `sp` / `regs[]`）と同形で書ける。

NetBSD については **x86_64/aarch64 両方の分岐を新規に追加する必要がある**（既存の
FreeBSD/OpenBSD 分岐を横展開するだけでは済まない、より大きな upstream 作業）。
いずれも upstream へ報告・PR するのが本筋。

## 関連

- B-52（OpenBSD の WASM を Pulley + MAP_STACK + OnDemand で動かした。x86_64 の話）
- B-49（FreeBSD の Docker クロスビルドで aws-lc-sys が組み立てられない。別件）
- F-140（NetBSD 対応。本チケットの NetBSD 追記は F-140 の実機検証から得られた知見）
