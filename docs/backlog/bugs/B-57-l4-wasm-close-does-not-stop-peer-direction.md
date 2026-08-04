# B-57: L4 WASM network filter の `close_stream` が片方向しか止めず、backend 応答がクライアントへ漏れる

**状態: 修正済み**

## 事象

FreeBSD 実機の 4 並列フル E2E スイートで `test_l4_wasm_close_on_marker` が失敗する
（単体実行では成功する）。クリーンな環境（`proxy.log` に "Address already in use" が
一切無く、残存プロセスによる汚染ではないことを確認済み）で再現:

```
542 passed; 1 failed
---- test_l4_wasm_close_on_marker stdout ----
panicked at tests/e2e_tests.rs:17498:13:
WASM close_stream should prevent any HTTP response from being forwarded,
got 97 bytes: "HTTP/1.1 200 OK\r\n...Content-Length: 0\r\n\r\n"
```

その時の `proxy.log`（要旨）は WASM フィルタ自体は正しく動作したことを示している:

```
01:55:10.042  [network-filter] new connection, context=2
01:55:10.079  [network-filter] CLOSE_ME marker seen, closing downstream
01:55:10.086  [network-filter] downstream closed, context=2
01:55:10.089  [network-filter] upstream closed, context=2
```

マーカーは検出され、close 要求も出ている。にもかかわらずクライアントは
backend の 200 OK を受け取ってしまっていた。検出 (`.079`) から downstream 側の
クローズ完了 (`.086`) まで 7ms のギャップがある点にも注目。

## 調査

### 再現テストの構造

`test_l4_wasm_close_on_marker`（`tests/e2e_tests.rs`）は L4 WASM リスナーへ
`POST` リクエストのヘッダとボディ（`CLOSE_ME`）を **別々の `write_all`** で送る。
プロキシ側のヘッダ受信 → backend への転送 → backend からの早期応答、と
ボディ内のマーカー検出が競合し得る構造になっている。

### 真因: `bidirectional_forward` の WASM 経路に方向間の共有キャンセルが無い

`src/l4/proxy.rs::bidirectional_forward` の `#[cfg(feature = "wasm")]` 分岐:

```rust
let (c2u_bytes, u2c_bytes) = futures::join!(
    forward_direction_wasm(&client, &upstream, ..., true),
    forward_direction_wasm(&upstream, &client, ..., false)
);
```

client→upstream（downstream データ）と upstream→client（upstream データ）の
2 方向は `futures::join!` で協調動作するだけで、**互いの状態を一切共有しない**。
`forward_direction_wasm` 内で `NetworkFilterResult::Close` を受け取った方は
自分のループを `break` するだけ（旧コード、`src/l4/proxy.rs:376-379` 付近）で、
もう一方の方向には何も伝わらない。`futures::join!` は両方の Future が完了する
まで待つため、downstream 方向が close してもすでに backend から届いていた
（あるいは届く途中だった）応答を upstream→client 方向がそのままクライアントへ
転送し続けてしまう。

Proxy-Wasm の仕様上、`proxy_close_stream` は **コネクション全体** を終了させる
意味論であり、片方向だけを止める設計は誤りだった。

`on_new_connection` 時点の Close（`bidirectional_forward` 内 ~560 行目、
`bidirectional_forward_tls_terminate` 内 ~914 行目）は接続確立前に判定して即座に
`return` するため元々コネクション全体を終了させており問題ない。同様に
`bidirectional_forward_tls_terminate` のデータ転送ループは単一タスク内の
逐次ループ（client 読み取り→backend 読み取りを交互に 1 ループで回す）であり、
`break 'outer` がそのままコネクション全体を止めるため、こちらも対象外
（影響があるのは `bidirectional_forward` の非 TLS-terminate・WASM 有効経路のみ）。

## 修正

`src/l4/proxy.rs` に接続ごと 1 個の `Arc<AtomicBool>`（`close_requested`）を
導入し、`forward_direction_wasm` の両方向呼び出しへ共有で渡す。

- **共有フラグをループ 1 周につき 1 回 load**: 各方向の読み取りループの先頭で
  `close_requested` を確認し、立っていれば即座に転送を止める。
  `Arc<AtomicBool>` のアトミック load のみで、ホットパスへロック・アロケーションを
  追加しない（ホットパス絶対規則を遵守）。
- **フィルタ呼び出し（非同期）の直後にも再チェック**: `on_downstream_data`/
  `on_upstream_data` はホスト関数呼び出しを含む非同期処理で、この `.await` 中に
  対向方向が close 要求を出す可能性がある。書き込み直前に `close_requested` を
  再確認し、立っていればすでに読み取り済みのデータを転送せずに終了する。
  これが本チケットの主要な race を塞ぐ（読み取り自体は既に完了しているデータに
  対する防御であり、read のブロッキング解除だけでは防げない）。
- **ブロックした read を即座に起こすための shutdown**: 片方向が close する時点で
  もう一方が `src.read()` でブロックしている可能性がある（アイドルタイムアウトまで
  待たせたくない）。close する方向は自分の `src`/`dst` 双方に対して
  `shutdown(std::net::Shutdown::Both)` を発行する。`forward_direction_wasm` の
  2 つの呼び出しは `(src, dst) = (client, upstream)` と `(upstream, client)` で
  ちょうど入れ替わっているため、この 2 回の `shutdown` だけで両方向のソケットが
  読み書き不可になり、対向方向の `read` はブロックせず即座に 0 バイト/エラーで
  返る。`shutdown(2)` は非ブロッキングな syscall で新規 io_uring オペコードを
  要さない（既存の半クローズ実装 `dst.shutdown(Write)` と同じ手法の延長）。

非 WASM（`wasm_modules.is_empty()`）の splice/zero-copy 経路は完全に無変更
（接続確立時の `is_empty()` 判定 1 回のみで、以降は従来どおり）。
`bidirectional_forward_tls_terminate` も無変更（前述のとおり対象外）。

## 検証

- `cargo build --features full` / `cargo clippy --all-targets --features full`
  / `cargo fmt --all -- --check`: いずれもクリーン。
- 単体テスト `src/l4/proxy.rs` に `close_requested` フラグの伝播を検証する
  決定的テストを追加（ライブソケットを使わず `Arc<AtomicBool>` の共有・
  `Ordering::SeqCst` の可視性のみを検証。実際の TCP ソケット越しの race は
  ライブソケットが無いと再現できないため、E2E（`test_l4_wasm_close_on_marker`、
  既存のまま変更なし）が本チケットの主回帰テストとなる。VM 実機での確認は
  コーディネータ側で実施）。

## 関連

- F-133（L4 WASM network filter 本体）
- B-45（L4 半クローズの `shutdown(Write)` 伝搬。本チケットの `shutdown(Both)` は
  その延長線上の手法）
