# B-52: OpenBSD で WASM モジュールを実行すると veil がプロセスごと SIGSEGV する

## 事象

OpenBSD 7.9 amd64（QEMU VM、`--no-default-features --features full-openbsd`）で、
**WASM フィルタを適用したルートへ 1 リクエスト送っただけで veil が SIGSEGV で死ぬ**。

```
tests/e2e_setup.sh: line 1:  4499 Segmentation fault \
  VEIL_TLS_INSECURE=1 "${VEIL_BIN}" --config "${FIXTURES_DIR}/proxy.toml" > /tmp/proxy.log 2>&1
```

最小再現（バックエンドすら不要。`/health` は 200 を返す状態から）:

```
$ curl -sk -m 5 -o /dev/null -w "%{http_code}\n" https://127.0.0.1:8443/health
200
$ curl -skv -m 10 https://127.0.0.1:8443/wasm/     # header_filter を適用したルート
* Connection closed abruptly
```

`/tmp/proxy.log` には**リクエストに対応する行が 1 行も出ない**（アクセスログも
エラーログも無い）。WASM エンジンの初期化自体は成功している:

```
Loading WASM module: header_filter
Loaded WASM module 'header_filter' with capabilities: http_calls=false, upstreams=[]
WASM Filter Engine initialized successfully
WASM tick thread started
```

## E2E への影響（重要）

このクラッシュは **E2E の集計上ほとんど見えない**。veil が死ぬと以降のテストは
`is_e2e_environment_ready()` が false になって **skip（= "ok" 扱い）** されるため、
「506 passed / 27 failed」のように*ほぼ通っているように見える*。実際には
プロキシが途中で死んで残りが検証されていない。

## 調査（3 つの仮説を実測で否定済み）

コアダンプの backtrace は**シンボルが取れない**:

```
Program terminated with signal 11, Segmentation fault.
#0  0x00000e8c14e2ebf0 in ?? ()
```

PC は匿名 mmap 領域内（JIT / ファイバスタック相当のアドレス帯）。gdb はそこから
1 フレームも巻き戻せない。`dmesg` に W^X / MAP_STACK 違反の記録は無い。

| # | 仮説 | 対処 | 結果 |
|---|---|---|---|
| 1 | wasmtime のファイバスタックが `MAP_STACK` 無しで mmap されており、OpenBSD 6.4+ の「SP は MAP_STACK 領域を指すこと」強制に触れる | `wasmtime::StackCreator` を実装し `MAP_STACK` + ガードページでスタックを確保（`Config::with_host_stack`） | **変化なし**（SIGSEGV のまま） |
| 2 | ガードページ + SIGSEGV ハンドラによる境界チェック（signals-based traps）が OpenBSD で機能しない | `Config::signals_based_traps(false)` で明示的境界チェックへ切替 | **変化なし** |
| 3 | JIT の `mprotect(PROT_EXEC)` が W^X で拒否され、非実行ページへジャンプしている | ビルド先 `/usr/obj` を `mount -u -o wxallowed` で再マウント（既定では `/usr/local` のみ wxallowed） | **変化なし** |

1 と 2 の実装は検証できなかったため**リバート済み**（未検証の unsafe コードを
残さない方針）。手順と結論だけを本チケットに残す。

FreeBSD では同じ `full-freebsd`（wasm 込み）で **WASM の E2E がすべて通っている**ため、
BSD 一般の問題ではなく **OpenBSD 固有**。wasmtime の公式サポート対象に OpenBSD は
含まれていない。

## 暫定対応（実施済み）

- `Cargo.toml` の **`full-openbsd` から `wasm` を外した**。OpenBSD 配布物は
  WASM フィルタ非対応となる（他機能 = HTTP/1.1・HTTP/2・HTTP/3・gRPC・WebSocket・
  L4・圧縮・キャッシュ・レート制限・バッファリング・admin・アクセスログは動作する）。
- `tests/e2e_tests.rs` の `test_f62_wasm_http_call_*` 2 件に
  `#[cfg(feature = "wasm")]` を付けた（他の WASM E2E は既に feature gate 済みで、
  この 2 件だけ漏れていた）。
- README / README.ja / packaging/README に OpenBSD の制限として明記。

## 次にやるなら

1. `wasmtime` 単体の最小再現（veil を介さず `wasmtime` の hello world を OpenBSD で
   `async_support(true)` で実行する）を作り、veil 側の問題でないことを確定させる。
2. 上流（bytecodealliance/wasmtime）へ OpenBSD の状況を確認・報告する。
3. `Config::async_support(false)`（同期実行）でも落ちるかを見る。落ちないなら
   ファイバ経路が真因で、OpenBSD だけ同期 wasm 実行にする案が取れる
   （ただし engine 側は `call_async` 前提のため相応の改修が要る）。

## 関連

- B-51（OpenBSD の E2E テストバイナリが aws-lc-rs で SIGSEGV。**別件・修正済み**。
  こちらを直したことで本件が見えるようになった）
- B-53（`head -c` 非互換。同じ E2E 実行で併発した別件・修正済み）
- F-120 Phase 5（OpenBSD 対応 / pledge・unveil）、F-122（OpenBSD は ring）
