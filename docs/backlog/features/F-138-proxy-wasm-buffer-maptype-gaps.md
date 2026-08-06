# F-138: Proxy-Wasm `CallData`（BufferType=8）は host→guest 発呼経路が無いため常に空

## 背景

F-134 の適合度テスト（`tests/proxy_wasm_conformance.rs`）で、`BufferType::CallData`
（値 8、`proxy_call_foreign_function` / `proxy_on_foreign_function` のペアで
host↔guest 間の任意引数バッファをやり取りするための型）が
**未認識の BufferType として `BadArgument` を返していた**ことが判明した
（適合度としては不適合。「veil が対応しない機能」と「型自体を知らない実装ミス」は
別物であり、後者は修正すべき）。

## 対応（F-134 で実施済み）

- `src/wasm/constants.rs`: `CALL_DATA: i32 = 8` を定義。
- `src/wasm/host/buffers.rs`: `get_buffer`/`is_known_buffer_type` で
  `CallData` を認識するようにし、`proxy_get_buffer_bytes` は常に空バッファ
  （0 バイト、trap しない）を返す。`check_read_capability` は
  「中身が常に空でセキュリティ上の意味を持たない」ため常に許可。

## 残課題（本チケットのスコープ）

veil は `proxy_call_foreign_function` ホスト関数（ゲスト → host 発呼）と、
その延長で host → guest 側 `proxy_on_foreign_function` を**呼び出す配線**
（Envoy 拡張、ABI 上はオプション機能）を実装していない。そのため `CallData`
バッファは実行時に埋まることが無く、常に空である。

### なぜ本チケットのスコープで実装しないか

- `proxy_on_foreign_function` は ABI 仕様上オプション（Envoy の `Wasm Service`
  拡張向けの機能で、コア機能ではない）であり、veil の想定ユースケース
  （HTTP/L4 フィルタ・gRPC 中継）では使用実績・要望が無い。
- 実装するには「host 側から任意のタイミングでゲストの
  `proxy_on_foreign_function(context_id, function_id, data_size) -> Action`
  を呼び出す」新しい呼び出し方向（現状は全てゲスト → host のみ）が必要になり、
  `HostState`/`FilterEngine` に新しい実行コンテキストとエントリポイントを
  追加する規模の変更になる。ABI 適合度そのものへの影響（`BadArgument`
  だったのを `Empty` 相当にする）は F-134 で解消済みで、実行系（呼び出し経路）
  が無いこと自体は ABI 違反ではない（呼ばれなければ何もしないだけ）。

## 改修案（実装する場合）

1. `proxy_call_foreign_function` ホスト関数を新設（`src/wasm/host/` に
   `foreign_function.rs` 等）。呼び出しは `function_id`（文字列）+
   任意バイト列を受け取る。
2. `HostState`/`HttpContext` に `call_data: bytes::Bytes` フィールドを追加し、
   `proxy_get_buffer_bytes(CallData, ...)` がそれを返すようにする。
3. host 側から `proxy_on_foreign_function` を呼ぶ具体的なユースケースを
   決める必要がある（例: 設定ファイルで「起動時に一度だけ特定の
   foreign function を呼ぶ」等）。ユースケースが定まらないまま経路だけ
   足しても呼ばれることがなく、テストで検証できないため後回しにしている。

## 優先度

P3（要望が出た時点で着手。ABI 適合度としては現状で問題なし）。
