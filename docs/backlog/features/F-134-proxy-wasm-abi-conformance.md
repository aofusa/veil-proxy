# F-134: Proxy-Wasm ABI v0.2.1 適合度テストと発見バグの修正

## 目的

`docs/artifacts/f132_wasm_design.md` の「4. F-134」節で定めた 7 項目を
`tests/proxy_wasm_conformance.rs` で検証し、見つかった不適合・実バグを修正する。

## 現状の事実（対応前）

適合度テスト自体は先行コミット（`c050183`）で追加済みで、実行すると
**10 passed / 3 failed** だった。加えてコードレビューで L4 network filter の
クローズコールバックに実バグが見つかった。

## 改修内容

### 1. Status コードの適合（3 件のテスト失敗を解消）

Proxy-Wasm ABI v0.2.1 の `proxy_status_t` には veil 独自拡張の
`PROXY_RESULT_NOT_ALLOWED(13)` は存在しない。修正前は「読み取り専用の
MapType/BufferType への書き込み」や「未知の enum 値」でも capability チェックの
`_ => false` 分岐に落ちて `NOT_ALLOWED` を返しており、ABI 上は `BadArgument(2)`
であるべき場面で veil 拡張の値を誤用していた。

- `src/wasm/constants.rs`: `PROXY_RESULT_NOT_ALLOWED` のコメントを
  「capability による拒否専用」に確定させた。
- `src/wasm/host/headers.rs`: `is_writable_map_type`（0-3 のみ true）を追加し、
  `proxy_set_header_map_pairs`/`proxy_add_header_map_value`/
  `proxy_replace_header_map_value`/`proxy_remove_header_map_value` で
  capability チェックより前に判定する（読み取り専用型・未知型は
  capability の有無に関わらず `BadArgument`）。あわせて `is_known_map_type`
  （0-7）を追加し、`proxy_get_header_map_pairs`/`proxy_get_header_map_value` で
  「型は認識するがデータ未到着」（gRPC 受信メタデータ・HTTP call 応答が
  まだ届いていない）を `NotFound(1)` として区別する
  （完全に未知の型は従来どおり `BadArgument`）。
- `src/wasm/host/buffers.rs`: 同様に `is_writable_buffer_type`
  （0-3 のみ true）・`is_known_buffer_type`（0-8）を追加し、
  `proxy_get_buffer_bytes`/`proxy_set_buffer_bytes` の判定順を
  「型認識 → capability」に入れ替えた。

### 2. L4 クローズコールバックのエクスポート名の実バグ（重要）

`src/wasm/engine.rs` が `proxy_on_downstream_close`/`proxy_on_upstream_close`
（接尾辞 `_connection` 抜き）という**存在しない名前**を呼んでいた。
Proxy-Wasm ABI v0.2.1 の正しい名前は `proxy_on_downstream_connection_close`/
`proxy_on_upstream_connection_close`（`proxy-wasm-rust-sdk` の
`src/dispatcher.rs` で確認済み）。

`wasmtime::Instance::get_typed_func` は未知の名前に対して trap ではなく `Err`
を返すだけなので、`execute_on_network_close` は静かに no-op になっていた
（呼び出し元はエラーに気づけない）。**実 SDK でビルドしたモジュールでは
クローズコールバックが一度も発火しないバグ**だった。

調査の結果、`examples/wasm-filters/network-filter/`（`tests/fixtures/wasm/
network_filter.wasm`）は実 SDK（`proxy-wasm` crate 0.2.5）をそのまま使っており、
**フィクスチャ自体は最初から正しい名前でエクスポートしていた**
（SDK の `dispatcher.rs` が `extern "C" fn proxy_on_downstream_connection_close`
を生成するため）。したがって本チケットでは fixture の再ビルドは不要で、
`src/wasm/engine.rs` の呼び出し文字列のみを修正した。

- `src/wasm/engine.rs`: `on_downstream_close_with_modules`/
  `on_upstream_close_with_modules` の呼び出し名を修正。
- `README.md`/`docs/readme/README.ja.md`: L4 network filter 節の
  コールバック名一覧を修正。
- `tests/proxy_wasm_conformance.rs`:
  - `EXPECTED_GUEST_EXPORTS` に正しい名前を追加（5 フィクスチャすべてが
    エクスポートしていることを確認）。
  - `item5_close_callback_host_call_site_uses_correct_abi_name`
    （新規）: host 側（`src/wasm/engine.rs`）のソースを直接検査し、
    正しい呼び出し文字列が存在し、誤った文字列が存在しないことを確認する
    回帰テスト。export 存在確認だけでは「host が別の誤った名前を呼んでいる」
    ケースを検出できないため。

### 3. `proxy_grpc_*` 実行ループの TLS 上流対応

`src/wasm/host/grpc_executor.rs` の `execute_grpc_unary_call` に `use_tls`
引数を追加し、TLS 上流（gRPC over h2, ALPN `h2`）へ接続できるようにした。
`src/wasm/http_executor.rs::execute_https_request`（`proxy_http_call` の
TLS 実装）と同じ rustls + webpki-roots システムルート検証を踏襲し、
`ClientStream`（`Plain(TcpStream)` / `Tls(StreamOwned<ClientConnection,
TcpStream>)`）で `Read`/`Write` を共通化した。呼び出し元
`src/server.rs`（WASM tick スレッドの gRPC 実行ループ）は `proxy_http_call`
と同じ `server.use_tls()` を使う。

真の逐次双方向ストリーミング・接続プーリングは未対応のまま
（理由は F-139 参照）。

## 検証

```bash
cargo test --features full --test proxy_wasm_conformance -- --nocapture --test-threads=1
# => 14 passed; 0 failed
cargo test --lib --features full
cargo build --features full
cargo clippy --features full --all-targets -- -D warnings
```

適合度レポート（`docs/artifacts/proxy_wasm_conformance_report.md`）は
**実装済み 12 / 未実装（意図的） 1 / 部分実装 1** に収束した。

- 未実装（意図的） 1件: BufferType 8（CallData）— host→guest の
  `proxy_on_foreign_function` 呼び出し経路を veil が実装していないため常に空
  （F-138 参照）。
- 部分実装 1件: `proxy_grpc_call` 系の真の双方向ストリーミング・接続プーリング
  （F-139 参照）。

## 既知の限界・後続チケット

- [F-138](F-138-proxy-wasm-buffer-maptype-gaps.md): `CallData`（BufferType=8）
  は host→guest 発呼経路が無いため常に空。
- [F-139](F-139-wasm-grpc-call-execution.md): `proxy_grpc_*` の真の双方向
  ストリーミング・接続プーリング。
