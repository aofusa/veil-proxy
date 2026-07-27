# B-52: OpenBSD で WASM モジュールを実行すると veil がプロセスごと SIGSEGV する

**状態: 修正済み（2026-07-27）**

## 事象

OpenBSD 7.9 amd64（QEMU VM、`--no-default-features --features full-openbsd`）で、
**WASM フィルタを適用したルートへ 1 リクエスト送っただけで veil が SIGSEGV で死ぬ**。

```
tests/e2e_setup.sh: line 1:  4499 Segmentation fault \
  VEIL_TLS_INSECURE=1 "${VEIL_BIN}" --config "${FIXTURES_DIR}/proxy.toml" > /tmp/proxy.log 2>&1
```

最小再現（バックエンド不要。`/health` は 200 を返す状態から）:

```
$ curl -sk -m 5 -o /dev/null -w "%{http_code}\n" https://127.0.0.1:8443/health
200
$ curl -skv -m 10 https://127.0.0.1:8443/wasm/     # header_filter を適用したルート
* Connection closed abruptly
```

`/tmp/proxy.log` には**リクエストに対応する行が 1 行も出ない**。WASM エンジンの
初期化自体は成功している。

## E2E への影響（重要・再発時の注意）

このクラッシュは **E2E の集計上ほとんど見えない**。veil が死ぬと以降のテストは
`is_e2e_environment_ready()` が false になって **skip（= "ok" 扱い）** されるため、
「506 passed / 27 failed」のように*ほぼ通っているように見える*。
**E2E のログに `Segmentation fault` が出ていないかを必ず確認すること。**

## 真因

**OpenBSD 6.4 以降は「スタックポインタが `MAP_STACK` 付きでマップされた領域を
指していること」をカーネルが強制する。** 条件を満たさないままカーネルへ入ると
プロセスは SIGSEGV で殺される。

`wasmtime::Config::async_support(true)` を使うと wasm の実行は **ファイバ**
（wasmtime が確保した専用スタックへスタックスイッチして実行する仕組み）の上で行われる。
wasmtime のファイバスタックは `MAP_STACK` 無しの通常の `mmap` で確保されるため、
**ファイバへ切り替えた瞬間に落ちる**。

### 決め手になった ktrace

```
 53005 veil  CALL  mprotect(0xe62593b000,0x110000,0x3<PROT_READ|PROT_WRITE>)
 53005 veil  RET   mprotect 0
 53005 veil  CALL  mmap(0,0x80000,0x3<PROT_READ|PROT_WRITE>,0x1002<MAP_PRIVATE|MAP_ANON>,-1,0)
 53005 veil  PSIG  SIGSEGV caught handler=0xe328cf3310 code=SEGV_ACCERR addr=0xe326d922c0
 53005 veil  CALL  sigaction(SIGSEGV,0xe5d789bae0,0)
 53005 veil  CALL  sigreturn(0xe5d789bb40)
 53005 veil  RET   sigreturn JUSTRETURN
 53005 veil  PSIG  SIGSEGV SIG_DFL code=SEGV_ACCERR addr=0xe326d922c0
```

- `code=SEGV_ACCERR`（**マップ済みだがアクセス不許可**。未マップなら `SEGV_MAPERR`）
- `addr=0xe326d922c0` は **ページ境界に揃っていない** → コード/データではなく
  **スタックポインタ**の値。これが `MAP_STACK` 強制の典型的なシグネチャ。
- wasmtime の SIGSEGV ハンドラが一度捕捉し、「wasm のトラップではない」と判断して
  `SIG_DFL` に戻して再送 → プロセス終了、という流れも読み取れる。

### なぜ「MAP_STACK 対応」が最初は効かなかったのか（重要）

`Config::with_host_stack`（ファイバスタック確保の差し替え）は
**`InstanceAllocationStrategy::OnDemand` でしか参照されない**。
veil は**プーリングアロケータ**を使っていたため、wasmtime 40 の
`config.rs::build_allocator` は `set_stack_creator` を呼ばず、**指定を黙って捨てる**:

```rust
match &self.allocation_strategy {
    InstanceAllocationStrategy::OnDemand => {
        let mut _allocator = Box::new(OnDemandInstanceAllocator::new(...));
        #[cfg(feature = "async")]
        if let Some(stack_creator) = &self.stack_creator {
            _allocator.set_stack_creator(stack_creator.clone());   // ← ここだけ
        }
        Ok(_allocator)
    }
    InstanceAllocationStrategy::Pooling(config) => { /* stack_creator を見ない */ }
}
```

プーリング側はスタックを自前のスラブから切り出す（`pooling.rs::allocate_fiber_stack`
→ `self.stacks.allocate()`）。つまり最初に入れた `StackCreator` は
**一度も呼ばれていなかった**。効果が無かったのは仮説が誤りだったからではなく、
**仮説を検証できていなかったから**である。

## 修正

OpenBSD のみ 3 点（他ターゲットは一切変更なし）:

1. **`InstanceAllocationStrategy::OnDemand`** を使う（`with_host_stack` を効かせるため）。
2. **`MAP_STACK` 付きファイバスタック**（`src/wasm/openbsd_stack.rs` の
   `MapStackCreator`）。低位側に `PROT_NONE` のガードページを 1 枚置き、
   その上を `MAP_FIXED | MAP_STACK` で貼り直す（`MAP_STACK` は mmap 時にしか付けられず、
   `mprotect` では後付けできない）。
3. **Pulley インタープリタ**（`Config::target("pulley64")` + wasmtime の `pulley` feature）。

3 が必要な理由は「クラッシュの回避」ではなく **配布上の都合**である。OpenBSD の W^X は
JIT が `mprotect(PROT_EXEC)` するとき、**実行ファイルが `wxallowed` マウント上に
あること**を要求する（既定では `/usr/local` のみ）。ネイティブ JIT のままだと
veil の設置場所に制約が生まれるため、ネイティブコードを一切生成しない Pulley を選ぶ。
代償として wasm の実行速度はインタープリタ相当になる。

## 検証

```
tools/qemu/bsd-vm.sh openbsd x86_64 ssh '... TEST_FILTER=test_f62_wasm ... e2e_setup.sh test'
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 531 filtered out
```

修正前は同じコマンドで veil が SIGSEGV し 0 passed / 2 failed だった。

## 否定された仮説（記録）

| # | 仮説 | 結果 |
|---|---|---|
| 1 | `signals_based_traps(false)`（ガードページ + シグナル経由のトラップが原因） | 効果なし |
| 2 | `/usr/obj` が `wxallowed` でない（W^X で JIT が失敗） | 実際に非 wxallowed だったが、再マウントしても効果なし |
| 3 | Pulley 単体（ネイティブコード生成が原因） | 効果なし（= 真因はコード実行方式ではない、と分かった有用な否定） |
| 4 | Pulley + `MAP_STACK`（プーリングアロケータのまま） | 効果なし（上記のとおり `StackCreator` が無視されていた） |

## 関連

- B-51（OpenBSD の E2E テストバイナリが aws-lc-rs で SIGSEGV。別件・修正済み。
  こちらを直したことで本件が見えるようになった）
- B-53（`head -c` 非互換。同じ E2E 実行で併発した別件・修正済み）
- F-120 Phase 5（OpenBSD 対応 / pledge・unveil）、F-122（OpenBSD は ring）
