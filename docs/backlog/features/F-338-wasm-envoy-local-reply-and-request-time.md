# F-338: Proxy-Wasm の Envoy との互換を確かめる（ローカル応答が応答の段を通るか・`request.time` と数値のプロパティの形式）

**状態: 未着手（要確認。[F-327](F-327-proxy-wasm-path-cost.md) の調査で見つけた。2026-11-26）**

拡張性の方針（Envoy の Proxy-Wasm で動くフィルタがそのまま veil で動く。F-327）に関わる違いの候補。まず Envoy の動きを確かめ、
違えば直し方を決める。

## 候補

1. **ローカル応答が応答の段を通るか**: Envoy はローカル応答（フィルタや Envoy 自身が返す 4xx 等）も応答の段
   （`proxy_on_response_headers` 等）を通すと見ている（`FilterManager::sendLocalReplyViaFilterChain` が先頭の encoder フィルタから
   符号化する）。veil は通さない。確かめること: Wasm のフィルタ自身が `proxy_send_local_response` で返した応答で、同じフィルタ・
   前後のフィルタの応答の段が呼ばれるか（Envoy の版による違いも）。
2. **`request.time` の値**: veil は呼んだときの時刻を秒の 10 進の文字列で返す（`src/wasm/host/properties.rs`）。Envoy の属性の
   定義は要求の最初のバイトを受けた時刻で、型はタイムスタンプ。確かめること: Envoy が Proxy-Wasm へ渡す値の形式（バイト列の
   形）と、Rust の SDK のフィルタが読む形。

## Envoy の動き（Gemini の読み。`docs/artifacts/proxy-wasm-perf/gemini/ans-12-review-b504-fix.md` の 5。実装の前にソースで確かめる）

1. ローカル応答:
   - 応答ヘッダの段がまだ始まっていないとき（要求の段のフィルタの `sendLocalReply`・経路が無い 404・バッファの上限の
     413 等）のローカル応答は、encoder のフィルタ（応答の段）を全部通る（`FilterManager::sendLocalReply` →
     `prepareLocalReplyViaFilterChain` → `sendLocalReplyViaFilterChain`）。
   - Wasm のフィルタが要求の段で `proxy_send_local_response` を呼んだときも、VM の呼び出しの後に `sendLocalReply` が走り、
     そのフィルタ自身を含めて `proxy_on_response_headers`・`proxy_on_response_body` が呼ばれる
     （`source/extensions/common/wasm/context.cc` の `Context::sendLocalResponse`。自分の応答の段への再入のハングは Envoy の
     issue #38801・PR #40166）。
   - encoder のフィルタが動き始めた後のローカル応答はフィルタを通さずに送り（`sendDirectLocalReply`）、応答ヘッダを送った
     後ならストリームをリセットする。
   - veil で合わせるなら、応答の段でまたローカル応答を返したときの打ち切り（`sendDirectLocalReply` に当たるもの）と、
     F-329 の 1 回の出入りの経路（要求の段のローカル応答の後すぐログ・削除）との関係を決める（応答の段を持たないモジュールには
     影響しない）。
2. `request.time`: 要求の最初のバイトを受けた時刻（`StreamInfo::startTime()`）。`serializeValue` は Unix エポックからの
   ナノ秒を 8 バイトのリトルエンディアンの `int64` で渡す。Rust の SDK のフィルタは `get_property(vec!["request", "time"])`
   の 8 バイトを `i64::from_le_bytes` で読む。veil は意味（呼んだ時刻）・単位（秒）・形式（10 進の文字列）の 3 つとも違う。
3. **数値の型のプロパティ**（候補に足す）: Envoy は `kInt64`・`kUint64`（`response.code`・`request.size`・
   `request.total_size` 等）も 8 バイトのリトルエンディアンの整数で渡す。veil は `request.total_size` 等を 10 進の文字列で返す
   （`src/wasm/host/properties.rs`）。[F-336](F-336-wasm-on-log-response-values.md)（`response.code` を見せる）と一緒に、Envoy の形式に
   揃えるか、今のフィルタのために文字列のままにしてガイドに書くかを決める。

出典（Gemini の挙げたもの）: <https://github.com/envoyproxy/envoy/blob/main/source/common/http/filter_manager.cc>・
<https://github.com/envoyproxy/envoy/blob/main/source/extensions/common/wasm/context.cc>・
<https://www.envoyproxy.io/docs/envoy/latest/intro/arch_overview/advanced/attributes>

## 受け入れ条件

- 各候補について Envoy の動き（ソースの箇所・版）を書き、違うものは直すか、直さない理由をガイド（英日）の Proxy-Wasm の章に書く。
- 直すものは、今のフィルタが今できることを減らさない（読み方が変わる値は移行の手順を書く）。

## 関連

- [F-327](F-327-proxy-wasm-path-cost.md)（調査で見つけた）・[F-336](F-336-wasm-on-log-response-values.md)（`proxy_on_log` で見える値）・
  B-300（応答ヘッダの段のローカル応答）
