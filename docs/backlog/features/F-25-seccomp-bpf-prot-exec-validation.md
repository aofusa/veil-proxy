# F-25: seccomp BPF フィルタの引数レベル検証（PROT_EXEC ブロック）

## 概要

`src/security.rs` の seccomp BPF フィルタを拡張し、`mprotect(2)` および `mmap(2)` で `PROT_EXEC` フラグが指定されたとき、Proxy-Wasm 用メモリ以外でのページを BPF 内でブロックする。

## 現状

- `mmap` (9) / `mprotect` (10) が無条件で許可されている
- wasmtime JIT が実行可能メモリを確保するため完全拒否はできない
- バッファオーバーフロー等の脆弱性があった場合、攻撃者が `PROT_EXEC` を立てて任意コード実行するパスが残っている

## 改修内容

### BPF 引数レベルフィルタの追加

seccomp BPF は `seccomp_data` 構造体の `args[i]` (各 8 バイト, オフセット `16 + i * 8`) を参照できる。

`mprotect` / `mmap` に対して以下のチェックを追加:

1. システムコール番号が `mprotect` (10) または `mmap` (9) であることを確認
2. `prot` 引数（`mprotect` は arg1, `mmap` は arg2）の下位 32 bit を読み込む
3. `PROT_EXEC` (0x4) フラグが立っている場合は `SECCOMP_RET_ERRNO | EPERM` を返す

### WASM 用メモリの除外

wasmtime は seccomp 適用前にメモリを確保するため、seccomp 適用前に wasmtime エンジンを初期化することで WASM 用メモリの確保を回避できる。

### BPF 命令例（x86_64）

```
# mprotect の prot 引数チェック
BPF_LD  BPF_W BPF_ABS  k=0     # syscall nr
BPF_JMP BPF_JEQ BPF_K  k=10 jt=1 jf=skip  # mprotect?
BPF_LD  BPF_W BPF_ABS  k=24    # arg1 (prot) 低32bit
BPF_JMP BPF_JSET BPF_K k=4 jt=deny jf=allow  # PROT_EXEC set?
```

## 改修案

`build_seccomp_filter()` 関数にシステムコール番号 `mprotect` / `mmap` のケースを追加し、引数ロード + `JSET PROT_EXEC` でブロック命令を挿入する。

## 受け入れ条件

- [ ] `mprotect` + `PROT_EXEC` が `EPERM` でブロックされる（WASM 初期化後）
- [ ] `mprotect` + `PROT_READ|PROT_WRITE` は通過する
- [ ] wasmtime が動作する（seccomp 適用前に初期化）
- [ ] `cargo test --features "full"` が通る

## 依存・リスク

- seccomp BPF は `seccomp_data.args[]` に 64-bit 値を 2 つの 32-bit ワードとして格納する（アーキテクチャ依存）
- wasmtime の JIT がランタイム中に `mprotect + PROT_EXEC` を呼ぶ場合は初期化順序の調整が必要

## 優先度

P1（セキュリティ強化）
