# B-55: wasmtime が BSD × aarch64 をサポートしておらず `full-freebsd` / `full-openbsd` がビルドできない

**状態: 回避済み（BSD aarch64 は wasm 抜きの feature セットを使う）**

## 事象

FreeBSD 14.3 arm64（QEMU VM）で `--no-default-features --features full-freebsd`
（= `wasm` を含む）をビルドすると、依存の wasmtime でコンパイルエラーになる。

```
error: unsupported platform
   --> /root/.cargo/registry/src/index.crates.io-.../wasmtime-40.0.4/
       src/runtime/vm/sys/unix/signals.rs:401:13
    |
401 |             compile_error!("unsupported platform");
```

## 原因

wasmtime のシグナルベーストラップ実装は、トラップ時に `ucontext` を書き換えて
実行を復帰させるため **OS × アーキごとのレジスタ名を直に触る**。その分岐が
`cfg_if` で列挙されており、**BSD は x86_64 しか無い**:

| 分岐 | 対応 |
|---|---|
| `linux` × `x86_64` / `aarch64` / `s390x` / `riscv64` | ✅ |
| `apple`（macOS/iOS）× `x86_64` / `aarch64` | ✅ |
| **`freebsd` × `x86_64`** | ✅ |
| **`openbsd` × `x86_64`** | ✅ |
| **`freebsd` × `aarch64`** | ❌ `compile_error!` |
| **`openbsd` × `aarch64`** | ❌ `compile_error!` |

さらにこの経路は **cargo feature では無効化できない**。`signals.rs` は
`#[cfg(has_native_signals)]` で囲われており、`has_native_signals` は wasmtime 自身の
`build.rs` が **ホストの `CARGO_CFG_TARGET_ARCH`** から導出する:

```rust
let has_host_compiler_backend = matches!(target_arch, "x86_64" | "riscv64" | "s390x" | "aarch64");
let has_native_signals = !miri && (supported_os || cfg!(feature = "custom-native-signals"))
    && has_host_compiler_backend;
```

aarch64 は `has_host_compiler_backend = true` なので、BSD aarch64 でも
`has_native_signals` が立ち、対応分岐が無いまま `signals.rs` がコンパイルされる。

**Pulley（B-52 で OpenBSD に導入したインタープリタ）でも回避できない。**
Pulley は `Config::target("pulley64")` による**コード生成先**の切り替えであって、
上記 `build.rs` の判定は**ホストの target_arch** を見るため影響しない。

## 影響

- BSD × aarch64 では **Proxy-Wasm 拡張が使えない**。
- それ以外（HTTP/1.1・HTTP/2・**HTTP/3**・gRPC・WebSocket・L4・圧縮・キャッシュ・
  レート制限・バッファリング・admin・アクセスログ・kTLS(FreeBSD)・AIO(FreeBSD)）は
  BSD aarch64 でも利用できる。
- BSD × **x86_64** は影響なし（従来どおり wasm 込み）。

## 対応

cargo は **feature セットを target 別に切り替えられない**（`[target.*]` は依存関係
専用で features には効かない）ため、既存の `full-freebsd` / `full-openbsd` と同じく
**arch 別の feature セットを明示指定する**方式にそろえた。

- `Cargo.toml` に `full-freebsd-aarch64` / `full-openbsd-aarch64` を追加
  （それぞれ `full-freebsd` / `full-openbsd` から **`wasm` だけを除いたもの**）。
- `tools/qemu/bsd-vm.sh` の `_default_features()` が `ARCH=aarch64` のとき
  自動で `-aarch64` 付きのセットを選ぶ。

## 上流について

wasmtime に `(freebsd, aarch64)` / `(openbsd, aarch64)` の `ucontext` 分岐を足せば
解消する見込み。FreeBSD arm64 の `mcontext_t` は `mc_gpregs.gp_elr` / `gp_sp` /
`gp_x[]` を持つため、Linux aarch64 の分岐（`uc_mcontext.pc` / `sp` / `regs[]`）と
同形で書ける。upstream へ報告・PR するのが本筋。

## 関連

- B-52（OpenBSD の WASM を Pulley + MAP_STACK + OnDemand で動かした。x86_64 の話）
- B-49（FreeBSD の Docker クロスビルドで aws-lc-sys が組み立てられない。別件）
