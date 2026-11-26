# F-334: HttpArena のエントリの Proxy-Wasm のフィルタ（拡張の側）を Wasm 向けに最適化し、フィルタを書く人向けの性能の注意をガイドに足す

**状態: 完了（2026-11-26、feat/proxy-wasm-perf。採用は 2026-11-26 利用者。[F-327](F-327-proxy-wasm-path-cost.md) の子。veil 本体のコード・設定・機能は変えない。フィルタは手元〈`docs/artifacts`〉、提出はしていない）**

## 範囲

- 変えるのは、HttpArena へ出すエントリのフィルタ（`docs/artifacts/benchmark/entries/httparena/veil/filter/`。veil のクレートの外の Rust の
  クレート）とそのビルド（エントリの `Dockerfile` の `filter` の段）、ガイド（`docs/guide/` の Proxy-Wasm の章、英日）の文書だけ。veil 本体の
  コード・設定・機能は変えない（利用者の方針: HttpArena 向けに最適化した Wasm を用意するのはよいが、veil 本体の機能を減らして拡張性を下げない）。
- フィルタは標準の Proxy-Wasm の ABI（0.2.1）のホスト関数と WASI だけを使い、veil に固有の機能に頼らない（同じ .wasm が他のホストでも
  同じ意味で動く形を保つ）。HttpArena の規則（応答本文は要求ごとに直列化する等）も守る。

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

1. SDK のまま（proxy-wasm の Rust の SDK 0.2.5）: `put_i64` をインライン化し、出力の `Vec` の空き容量へ直接
   2 桁ずつの表で書く（ゼロ埋めと写しを無くす。ループの回数が半分になればエポックの検査も半分）、要求ごとの確保を減らす（固定の引数は
   静的に）、bulk-memory 無しでビルドする（`RUSTFLAGS='-C target-feature=-bulk-memory,-bulk-memory-opt'`。エントリの Dockerfile。
   測った値は json-tls −2.2 µs。根拠は [F-333](F-333-wasm-bulk-memory-libcalls.md) の「ゲストの側の試し」）。
   `active` は分岐の中でそれぞれ `extend_from_slice` する。`write_body` は 1 行の最大長を先に確保し、手元のカーソルで書いて最後に長さを置く。
   POST はヘッダの段で読んだ値を文脈に持ち、本文の段でパスを引き直さない。`set_http_context` を使う。
   ans-03 の見込み（推測）: bulk-memory 無し −2.0〜2.7（測った値は −2.2）、`put_i64` −0.9〜1.2、カーソル書き −0.25〜0.45（いずれも json-tls）、
   SDK のままの小さな改善は GET −0.05〜0.10・POST −0.25〜0.40 µs。
2. SDK を使わずに Proxy-Wasm の ABI を直接実装した版も作る（固定のプロパティのパスとヘッダを定数のバイト列に、文脈の表と `Box<dyn>` と
   要求ごとの確保を無くす。ans-03 の見込みで h1・h2 −0.35〜0.45 µs/要求）。1 との差を測って記録し、どちらを提出に使うかは提出のときに
   利用者が決める（Rust の SDK で書いたフィルタがそのまま動くことを見せるなら 1、点数なら 2。どちらも標準の ABI）。
3. ガイド（`docs/guide/` の Proxy-Wasm の章、英日）に、フィルタを書く人向けの性能の注意を足す（ans-03 の 5-2 から選ぶ）: `instance_reuse`、
   bulk-memory と小さな写し（Rust・TinyGo・Clang の既定。Rust は上のフラグ、Clang は `-mno-bulk-memory -mno-bulk-memory-opt`）、長さの違う
   定数を 1 つの写しに畳まない書き方、コールバックごとの確保と `get_property` の戻り値の確保・前の段で読んだ値を持ち越す、ループの反復の
   回数とエポックの検査（小さな関数のインライン化）。

## 受け入れ条件

- aofish の交互 A/B で、SDK のまま（1）で json-tls の 1 要求のユーザー時間が 3.0 µs 以上（bulk-memory 無しのビルドを含む）、POST が 0.2 µs 以上
  下がる。h1 GET の 0.2 µs 以上は SDK を外す場合（2）の条件にする（SDK のままの見込みは 0.05〜0.10 µs）。
- 2 を作り、1 との差（1 要求のユーザー時間・rps）を記録する。
- 1・2 とも、フィルタが import するのは Proxy-Wasm の ABI のホスト関数と WASI だけ（import の一覧を `wasm-tools print` 等で確かめる）。
- エントリの `validate.sh` が全件通る（`docs/artifacts/benchmark/` の手順。F-320 と同じ 72 件。1・2 とも）。
- ガイドの英日を同じ変更で更新する。

## 実装（2026-11-26）

フィルタ（`docs/artifacts/benchmark/entries/httparena/veil/filter/`、git の管理外、0.2.0）は、答え方を決める処理（`lib.rs`）・データ
セットを起動時に型付きの値へ解析して要求ごとに 1 項目ずつ書く処理（`json.rs`。使い回すバッファへ手元のカーソルで書き、`active` は
分岐ごとに別の固定の写し）・整数と小数を 2 桁ずつの表でその場に書く処理（`num.rs`。`put_i64` の別の関数・ゼロ埋め・写しを無くした）を
共有し、入り口を 2 つ持つ。どちらも bulk memory 無しでビルドする（`filter/.cargo/config.toml`。エントリの Dockerfile の `filter` の段は
`cargo build --release --locked --target wasm32-wasip1`）。

- 既定（2）: Proxy-Wasm の ABI（0.2.1）を直接実装（`abi.rs`・`mem.rs`）。ホストが返す値は 4 KiB までを静的な作業領域で受ける
  （`take_value` がホストを呼ぶ間だけ `proxy_on_memory_allocate` に貸す。大きい値〈データセット、1 回〉はヒープ）。応答ヘッダは直列化した
  形の定数。POST はヘッダの段で求めた `a + b` を context の ID で本文の段へ持ち越し（Envoy はヘッダの段が続けた後の本文の段に要求ヘッダを
  見せない）、`proxy_on_delete` で途中で終わった要求の分を消す。`proxy_on_vm_start`・`proxy_on_configure`・`proxy_on_context_create`・
  `proxy_on_log`・`proxy_on_done` はエクスポートしない。使い回すバッファが育った後は要求ごとの確保が無い。
- `--features sdk`（1）: Proxy-Wasm の Rust の SDK 0.2.5（`sdk.rs`）。`set_http_context` だけ（ルートの文脈の二重の検索が無い）。POST は
  ヘッダの段で読んだ値を文脈に持つ。`pool` は 16〜1024 バイトの空きの表をアロケータの前に置く（測った差が小さく、既定にはしない）。
- import はどちらも `env` の Proxy-Wasm のホスト関数と WASI だけ（`wasm-tools print` で確かめた。veil に固有の import は無い）。
  bulk memory 無しのビルドに残る `memory.copy`・`memory.fill` は 9 か所で、32 バイトを超える写しの wasi-libc の `memcpy`・`memset`。
- ガイド（`docs/guide/wasm.md`・`ja/wasm.md` の「6. Performance Tips」・「6. 性能のための注意」）: `instance_reuse`・使うコールバック
  だけのエクスポート・値を 1 回読んで持つ（Envoy は本文の段に要求ヘッダを見せない）・コールバックごとに確保しない・bulk memory 無しの
  ビルド（Rust・Clang のフラグ、rustc の警告）・写しの大きさを固定に保つ・ループの回数・1 回だけの仕事を別の関数へ。

## 計測（aofish、`~/hp/bench.py`、3 巡の中央値、1 要求のユーザー + システムの CPU µs）

1 回目（`f334a`。veil は 47a6e284〈F-329 の前〉、io_uring）:

| フィルタ | h1 GET | h1 GET（`h1-arena`） | h1 POST | json-tls | h2 | h3 |
|---|---|---|---|---|---|---|
| 元のもの（SDK 0.2.5、bulk memory あり） | 16.55 | 20.16 | 20.78 | 38.22 | 7.16 | 7.72 |
| 元のもの・bulk memory 無し | 16.60 | 19.51 | 20.90 | 32.87 | 7.13 | 7.62 |
| SDK（1、最初の版） | 16.45 | 19.73 | 20.14 | 27.27 | 7.05 | 7.59 |
| SDK + `pool`（最初の版） | 15.85 | 19.27 | 20.34 | 27.03 | 6.88 | 7.65 |
| ABI を直接（2、最初の版） | 14.78 | 18.47 | 18.72 | 23.12 | 5.93 | 7.11 |

2 回目（`f334b`。veil は 2ac41b26〈F-329〉、io_uring。`sdk2`・`abi2` は 1 回目の後にレビュー〈`gemini/ans-05-review-F-334.md`〉を
受けて直した今のフィルタ: must-fix の 1-1〈32 ビットの `usize` の掛け算の折り返し。容量は行ごとの和で求める〉・2-1〈Envoy はヘッダの段が
続けた後の本文の段に要求先を見せない。POST の `a + b` をヘッダの段で求めて context ごとに持つ〉・5-1〈48 バイトの固定の写しが LLVM に
長さが可変の `memcpy` へ畳まれ、1 要求 150 回の `memory.copy` になっていた。ワード単位の写しにした〉、should-fix の 1-2〈作業領域を
`take_value` の間だけ貸す〉・5-2〈データセットの解析を別の関数へ〉・5-3〜5-5〈POST の計算の二重・小さな数の桁数・要求先の判定と
クエリの解析をワード単位・1 回の走査に〉）:

| 負荷 | 元のもの | SDK（最初の版） | SDK（`sdk2`） | SDK + `pool`（`sdkpool2`） | ABI（最初の版） | ABI（`abi2`） |
|---|---|---|---|---|---|---|
| h1 GET（`h1-wasm`） | 15.53 | 15.29 | 15.16 | 14.92 | 14.31 | 13.73 |
| h1 GET（`h1-arena`） | 17.60 | 17.22 | 16.92 | 16.81 | 16.37 | 15.72 |
| h1 POST | 17.87 | 17.00 | 16.89 | 16.78 | 16.28 | 15.93 |
| json-tls | 35.57 | 24.47 | 19.97 | 19.43 | 21.55 | 18.73 |
| h2 | 6.68 | 6.51 | 6.28 | 6.19 | 5.71 | 5.62 |
| h3 | 7.41 | 7.25 | 7.31 | 7.16 | 6.86 | 6.87 |

json-tls（wrk、256 接続）は、どの版でも 3 巡のうち 1〜2 巡に wrk のソケットのエラーが 450〜490 件出る（1 巡 100〜250 万要求の
0.05% 以下、2xx 以外は 0。F-329 の前の veil〈47a6e284〉と元のフィルタでも出る）。中央値の巡に出たのは 2 回目の元のもの・`sdk2`。

プロファイル（`prof12.sh`、veil は ab4c0ead〈F-331 の後〉、io_uring、1 回ずつ。ユーザー時間 µs/要求、`guest:code` はゲストの
コード、`wt:libcall` は `memory.copy` 等のランタイムの呼び出し）:

| 負荷 | フィルタ | rps | ユーザー | システム | `guest:code` | `wt:libcall` |
|---|---|---|---|---|---|---|
| h1 GET | 元のもの | 522,759 | 5.83 | 8.60 | 0.772 | 0.085 |
| | SDK（`sdk2`） | 536,839 | 5.27 | 8.77 | 0.602 | 0 |
| | ABI（`abi2`） | 568,675 | 4.80 | 8.45 | 0.126 | 0 |
| h1 POST | 元のもの | 490,964 | 6.44 | 8.90 | 1.034 | 0.110 |
| | SDK（`sdk2`） | 517,544 | 5.88 | 8.71 | 0.742 | 0 |
| | ABI（`abi2`） | 548,022 | 5.12 | 8.68 | 0.224 | 0 |
| json-tls | 元のもの | 241,439 | 17.49 | 13.92 | 2.990 | 2.640 |
| | SDK（`sdk2`） | 416,203 | 8.80 | 9.07 | 1.460 | 0 |
| | ABI（`abi2`） | 571,160 | 8.08 | 8.59 | 0.890 | 0 |

受け入れ条件: SDK のまま（1）でユーザー時間が json-tls −8.69 µs（3.0 以上）、POST −0.56 µs（0.2 以上）。ABI を直接（2）で h1 GET
−1.03 µs（0.2 以上）。1 と 2 の差は h1 GET −0.47・POST −0.76・json-tls −0.72 µs（ユーザー時間）、rps は +6%・+6%・+37%（json-tls の
rps は wrk の TLS の揺れが大きい）。`pool` は SDK の版で −0.1〜0.5 µs だが、ABI の版には要らない（要求ごとの確保が無い）。

エントリの `validate.sh`（aofish の Docker、`VALIDATE_SKIP_BUILD=1`、F-320 と同じ 72 件）: エントリの Dockerfile の `filter` の段で
ビルドした既定（ABI）の版と、SDK の版（`sdk2`）のどちらも 72 件すべて通った（veil は ab4c0ead のバイナリ）。

## 関連

- [F-327](F-327-proxy-wasm-path-cost.md)（親）・[F-320](F-320-httparena-veil-entry-config.md)・[F-333](F-333-wasm-bulk-memory-libcalls.md)・`gemini/ans-03-guest.md`
