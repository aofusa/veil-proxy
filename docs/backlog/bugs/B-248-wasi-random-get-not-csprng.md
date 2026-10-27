# B-248: WASI の `random_get` が壁時計をシードにした LCG で、プラグインの乱数が予測可能

**状態: 未修正（2026-10-27 起票）**

## 出典

2026-10-27 のセキュリティレビュー 2 回目の WS-02。

## 事象

`src/wasm/host/wasi.rs` L279〜L310 の `random_get` は `SystemTime::now()` のナノ秒をシードに `seed * 6364136223846793005 + 1` の
上位 8 ビットをバイトごとに出す。Proxy-Wasm のプラグイン（Rust の `rand`・`getrandom`、TinyGo の `crypto/rand`、AssemblyScript）はこれを
暗号用の乱数源として使う。トークン・nonce・セッション ID・鍵（プラグインが JWT の署名や HMAC の鍵を作る場合）が予測可能になる。
同じナノ秒を知る必要はない: 出力は 8 ビットごとの LCG の上位バイトで、1〜2 バイトから内部状態を復元できる。

## 影響

- P2。プラグインが乱数に依存するときだけ。veil 本体の TLS・HMAC（HTTP/3 の CID）は `aws-lc-rs`/`ring` を使っていて影響しない。

## 改修案

- `getrandom(2)`（`security.rs` の `ALLOWED_SYSCALLS` に既にある）で埋める。`aws-lc-rs`/`ring` の `SystemRandom` でもよい（OpenBSD/NetBSD は ring）。
  BSD/macOS/Windows も `getrandom`/`arc4random_buf`/`BCryptGenRandom` で同じ（`getrandom` クレートはこれらを抽象化する。依存は既に tree にある）。
- 単体テスト: 2 回の呼び出しの出力が一致しない、バイトの分布が一様（カイ二乗の簡易検査）。

## 関連

- [B-247](B-247-wasm-dequeue-shared-queue-abi-writes-payload-not-pointer.md)、[F-282](../features/F-282-security-review-followup-hardening.md)
