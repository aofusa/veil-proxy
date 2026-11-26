# F-334: HttpArena の Proxy-Wasm のフィルタを Wasm 向けに書く（エントリの側）と、フィルタを書く人向けの性能の注意（ガイド）

**状態: 未着手（[F-327](F-327-proxy-wasm-path-cost.md) の子）**

## 現状

- フィルタ（`docs/artifacts/benchmark/entries/httparena/veil/filter/src/lib.rs`、proxy-wasm の Rust SDK 0.2.5、`wasm32-wasip1`）の
  ゲストの時間: h1 GET 0.75 µs/要求、json-tls 3.11 µs/要求。
- json-tls の上位: `put_i64` 7.33%（1.41 µs）。桁ごとの `u64` の `/ 10`・`% 10` で、Wasm には 64×64→128 の上位の乗算が無いので定数の割り算を
  掛け算に置き換えられないと見ている（ans-02・ans-03 で確かめる）。`write_body` 3.44%。
- h1 GET の上位: フィルタの `query_param`、dlmalloc・dlfree、SDK の `proxy_on_context_create`・`proxy_on_log`・`serialize_map`
  （`send_http_response` のヘッダ）・`create_http_context`・`serialize_property_path`。要求ごとに `vec!` と `String` を作っている
  （`get_property(vec!["request", "path"])`・`send_http_response` のヘッダの `vec!`・合計の `to_string()`）。
- HttpArena の規則: 応答本文は要求ごとに直列化する（使い回さない）。データセットを起動時に型付きの値へ解析しておくのは許される。

## 改修内容

1. フィルタ（エントリの側。veil のクレートの外。提出の前にエントリへ入れる）: `put_i64` を Wasm 向けに（`u32` に収まる値は `u32` で、2 桁ずつの表）、
   要求ごとの確保を減らす（固定の引数は静的に、数字は手元の配列へ）、写しの書き方（[F-333](F-333-wasm-bulk-memory-libcalls.md) の結果に合わせる）。
2. SDK を使わずに Proxy-Wasm の ABI を直接実装する案は、SDK の費用（文脈の表・`Box<dyn>`・確保）を測ってから判断する（「Proxy-Wasm を見せる」
   エントリとして SDK を使う方が説明しやすい。差が大きければ利用者に相談する）。
3. ガイド（`docs/guide/` の Proxy-Wasm の章、英日）に、フィルタを書く人向けの性能の注意を足す: `instance_reuse`、コールバックごとの確保、
   bulk-memory と小さな写し、64 ビットの定数の割り算、`get_property` の戻り値の確保（ans-03 の結果から選ぶ）。

## 受け入れ条件

- aofish の交互 A/B で json-tls のゲストの時間が 1.0 µs 以上、h1 GET が 0.2 µs 以上下がる。
- エントリの `validate.sh` が全件通る（`docs/artifacts/benchmark/` の手順。F-320 と同じ 72 件）。
- ガイドの英日を同じ変更で更新する。

## 関連

- [F-327](F-327-proxy-wasm-path-cost.md)（親）・[F-320](F-320-httparena-veil-entry-config.md)・[F-333](F-333-wasm-bulk-memory-libcalls.md)・`gemini/ans-03-guest.md`
