# B-55: wasmtime が BSD の一部プラットフォームをサポートしておらず `full-*` の一部がビルドできない

**状態: 回避済み（該当プラットフォームは wasm 抜きの feature セットを使う）**

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

## 対応

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
