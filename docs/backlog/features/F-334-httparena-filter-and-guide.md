# F-334: HttpArena の Proxy-Wasm のフィルタを Wasm 向けに書く（エントリの側）と、フィルタを書く人向けの性能の注意（ガイド）

**状態: 未着手（[F-327](F-327-proxy-wasm-path-cost.md) の子）**

## 現状

- フィルタ（`docs/artifacts/benchmark/entries/httparena/veil/filter/src/lib.rs`、proxy-wasm の Rust SDK 0.2.5、`wasm32-wasip1`）の
  ゲストの時間: h1 GET 0.75 µs/要求、json-tls 3.11 µs/要求。
- json-tls の上位: `put_i64` 7.33%（1.41 µs）、`write_body` 3.44%。`put_i64` の割り算は Cranelift が掛け算とシフトに置き換えている
  （`cranelift-codegen-0.123.21/src/opts/arithmetic.isle` の定数の `udiv`・`urem` の規則。ans-02 の 4。最初の見立て〈割り算が残る〉は誤り）。
  費用は、別の関数として 1 要求で数百回呼ばれる出入り（スタックの上限の検査・レジスタの退避・Wasm のシャドウスタック）、毎回の
  `[0u8; 20]` のゼロ埋め、桁ごとのエポックの検査（`epoch_interruption` がループの頭に入れる）とスライスの境界の検査、最後の
  `extend_from_slice` の `memory.copy`（ans-02 の 4-2。割合は推測）。
- h1 GET の上位: フィルタの `query_param`、dlmalloc・dlfree、SDK の `proxy_on_context_create`・`proxy_on_log`・`serialize_map`
  （`send_http_response` のヘッダ）・`create_http_context`・`serialize_property_path`。要求ごとに `vec!` と `String` を作っている
  （`get_property(vec!["request", "path"])`・`send_http_response` のヘッダの `vec!`・合計の `to_string()`）。
- HttpArena の規則: 応答本文は要求ごとに直列化する（使い回さない）。データセットを起動時に型付きの値へ解析しておくのは許される。
- ans-03（`docs/artifacts/proxy-wasm-perf/gemini/ans-03-guest.md`。SDK の二重の検索と `proxy_on_memory_allocate` の確保は SDK のソースで確かめた）:
  - GET 1 要求でゲストの確保と解放が 6 回ずつ（`vec!["request", "path"]`・`serialize_property_path`・ホストが返す値の
    `proxy_on_memory_allocate`・`to_string()`・ヘッダの `vec!`・`serialize_map`）、POST は 10 回ずつ（本文の段でパスをもう一度引く）。
  - SDK は `set_root_context` だけのフィルタで、文脈を作るたびに `roots` を 2 回引く（`dispatcher.rs` の `on_create_context`・`create_http_context`）。
    空の `on_log` も、ハッシュ表の検索と `Box<dyn HttpContext>` の間接呼び出しで約 0.05 µs かかる。
  - `write_body`: 長さが違う 2 つの定数を `if` で選ぶ書き方（`active` の `true`・`false`）は可変長の写しに畳まれて `memory.copy` になる。
    `OUT` が `thread_local!` なので、`Vec` の `ptr`・`len`・`cap` を `extend_from_slice` のたびに線形メモリから読み直す。

## 改修内容

1. フィルタ（エントリの側。veil のクレートの外。提出の前にエントリへ入れる）: `put_i64` をインライン化し、出力の `Vec` の空き容量へ直接
   2 桁ずつの表で書く（ゼロ埋めと写しを無くす。ループの回数が半分になればエポックの検査も半分）、要求ごとの確保を減らす（固定の引数は
   静的に）、bulk-memory 無しでビルドする（[F-333](F-333-wasm-bulk-memory-libcalls.md)）。
   `active` は分岐の中でそれぞれ `extend_from_slice` する。`write_body` は 1 行の最大長を先に確保し、手元のカーソルで書いて最後に長さを置く。
   POST はヘッダの段で読んだ値を文脈に持ち、本文の段でパスを引き直さない。`set_http_context` を使う。
   ans-03 の見込み（推測）: bulk-memory 無し −2.0〜2.7（測った値は −2.2）、`put_i64` −0.9〜1.2、カーソル書き −0.25〜0.45（いずれも json-tls）、
   SDK のままの小さな改善は GET −0.05〜0.10・POST −0.25〜0.40 µs。
2. SDK を使わずに Proxy-Wasm の ABI を直接実装する案（固定のプロパティのパスとヘッダを定数のバイト列に、文脈の表と `Box<dyn>` と要求ごとの
   確保を無くす）は、ans-03 の見込みで h1・h2 −0.35〜0.45 µs/要求。「Proxy-Wasm を見せる」エントリとして公式の SDK を使う方が説明しやすい
   ので、1 の後で測った差を示して利用者が決める。
3. ガイド（`docs/guide/` の Proxy-Wasm の章、英日）に、フィルタを書く人向けの性能の注意を足す: `instance_reuse`、コールバックごとの確保、
   bulk-memory と小さな写し、64 ビットの定数の割り算、`get_property` の戻り値の確保（ans-03 の結果から選ぶ）。

## 受け入れ条件

- aofish の交互 A/B で json-tls のゲストの時間が 1.0 µs 以上、h1 GET が 0.2 µs 以上下がる。
- エントリの `validate.sh` が全件通る（`docs/artifacts/benchmark/` の手順。F-320 と同じ 72 件）。
- ガイドの英日を同じ変更で更新する。

## 関連

- [F-327](F-327-proxy-wasm-path-cost.md)（親）・[F-320](F-320-httparena-veil-entry-config.md)・[F-333](F-333-wasm-bulk-memory-libcalls.md)・`gemini/ans-03-guest.md`
