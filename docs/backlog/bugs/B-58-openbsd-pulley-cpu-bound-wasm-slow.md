# B-58: OpenBSD（Pulley インタープリタ）で CPU 律速の WASM モジュールが極端に遅い

**状態: 調査完了・制限事項として文書化（機能不全ではない）**

## 事象

OpenBSD 7.9 amd64（QEMU 実機）の E2E で `test_http3_wasm_local_response` が
**単体実行でも** 20 秒の制限時間を超過して失敗する。

```
thread 'test_http3_wasm_local_response' panicked at tests/e2e_tests.rs:18741:1:
timeout: the function call took 20002 ms. Max time 20000 ms
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 541 filtered out
```

## 切り分け

### ハング／デッドロックではない

テストがタイムアウトした後も 90 秒待ってプロキシのログを確認したが、
エラーもトラップも記録されていない。実行が進行中のまま制限時間を迎えている。

実機ログのタイムスタンプ:

```
03:44:56.697  Loading WASM module: waf_filter
03:44:56.719  Loaded WASM module 'waf_filter'
03:45:14.159  [wasm:waf_filter] [waf] Using default configuration   ← 約 18 秒後
```

`on_configure`（ルートコンテキスト）へ到達するまでだけで **約 18 秒**を要している。
他プラットフォームでは数十 ms。

### HTTP/3 + WASM の経路自体は健全

同じ OpenBSD 実機で、軽量モジュール（`header_filter`）を使う HTTP/3 + WASM は
**単体 0.17 秒で完走**し、WASM のライフサイクルも最後まで記録される。

```
test_http3_wasm_request_header_mutation ... ok (0.17s)

[wasm:header_filter] Added request headers for context 2
[wasm:header_filter] Added response headers for context 2
[wasm:] Request 2 completed
```

したがって F-132（HTTP/3 の Proxy-Wasm 対応）自体に欠陥は無い。

### 原因

OpenBSD は W^X 制約のため WASM を Cranelift ネイティブ JIT ではなく
**Pulley インタープリタ**で実行する（B-52）。`waf_filter` は CRS Level 2 の
正規表現ルール群を評価する **CPU 律速**モジュールであり、インタープリタ実行の
オーバーヘッド（JIT 比で 10〜50 倍）がそのまま実行時間に乗る。

`header_filter` のようにヘッダを数個追加するだけのモジュールでは差が表面化しない。

## veil 側に修正余地が無いことの確認

高速化に使える手段を検討したが、いずれも採れない。

| 候補 | 可否 | 理由 |
|---|---|---|
| Cranelift JIT に切り替える | ✗ | OpenBSD の W^X 制約で実行可能 mmap を作れない（B-52） |
| プーリングアロケータを使う | ✗ | `Config::with_host_stack` は **OnDemand でしか参照されない**（wasmtime 40 `config.rs::build_allocator`）。プーリングは自前スラブから `MAP_STACK` 無しでスタックを切り出すため、ファイバへ切り替えた瞬間にカーネルに殺される（B-52） |
| `consume_fuel(false)` | ✗ | fuel は WASM の CPU 消費上限。**セキュリティ制御**なので外せない |
| `epoch_interruption(false)` | ✗ | 実行時間の上限保護。同上 |
| AOT キャッシュの活用 | 済 | 既に `.pulley.cwasm` を使用しており、実機ログでもモジュール読み込みは **22ms**（キャッシュヒット）。リクエストごとの再コンパイルは発生していない |

残る高速化手段は fuel 計測とエポック割り込みの除去のみで、これは修正ではなく
**セキュリティ機能の退行**にあたる。インタープリタが JIT より 10〜50 倍遅いのは
実行方式に内在する性質であり、veil 側のコードで解消できない。

したがって本件は**欠陥ではなくプラットフォームの性能特性**であり、
正確な記録が正しい解決である。

## 決定的な追加観測: QUIC の idle timeout が先に切れる

当初は「テストの制限時間が足りないだけ」と考え、OpenBSD のみ `ntest::timeout` を
20 秒 → 180 秒へ延長して実測した。結果、**制限時間の延長では解決しない**ことが判明した。

```
thread '<unnamed>' panicked at tests/e2e_tests.rs:18775:6:
HTTP/3 waf request: ConnectionError(Timeout)
test result: FAILED. 0 passed; 1 failed ... finished in 30.08s
```

180 秒の制限に達する前に、**30 秒で QUIC 接続そのものがタイムアウト**している
（`ConnectionError(Timeout)`）。WAF の評価が終わる前にクライアント側の QUIC idle
timeout が満了し、接続が切断される。

これは「遅い」ではなく **HTTP/3 上では機能しない**ことを意味する。テスト側の
制限時間をいくら延ばしてもトランスポート層のタイムアウトが先に効くため無意味である。

## 対応

`#[cfg_attr(target_os = "openbsd", ignore = "...")]` で OpenBSD のみ ignore する。
理由は属性内に明記し、無説明の抑制はしない。制限時間の延長は上記のとおり実測で
無効と確認済みのため採らない。

## 利用者への影響（重要）

* **OpenBSD で CPU 律速の WASM フィルタ（WAF、正規表現ルール群、大きなボディの走査）を
  HTTP/3 と組み合わせると、QUIC 接続がタイムアウトして応答できない。**
  遅いだけでなく実質的に利用できない。
* HTTP/1.1 / HTTP/2 はトランスポート層のタイムアウトが緩いため、遅いながらも完了しうる
  （本リポジトリに H1/H2 の WAF E2E は無いため未計測）。
* **ヘッダの追加・書き換え程度の軽量なフィルタは OpenBSD でも実用範囲**。
  同じ HTTP/3 経路で `header_filter` は 0.17 秒で完走する。
* NetBSD は wasmtime 40 が全アーキテクチャで非対応のため WASM 自体を利用できない（B-55）。
* Linux / FreeBSD / macOS / Windows は Cranelift JIT のため影響を受けない。

## 関連

- B-52: OpenBSD で MAP_STACK ファイバスタック + OnDemand + Pulley により WASM を動作させた
- B-55: wasmtime 40 が FreeBSD/OpenBSD aarch64 と NetBSD 全アーキテクチャで WASM 非対応
- F-135: `[wasm] interpreter` オプション（OpenBSD/NetBSD は設定値を無視して常に Pulley）
