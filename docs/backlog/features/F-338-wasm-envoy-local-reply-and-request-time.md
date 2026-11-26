# F-338: Proxy-Wasm の Envoy との互換を確かめる（ローカル応答が応答の段を通るか・`request.time` の値）

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

## 受け入れ条件

- 各候補について Envoy の動き（ソースの箇所・版）を書き、違うものは直すか、直さない理由をガイド（英日）の Proxy-Wasm の章に書く。
- 直すものは、今のフィルタが今できることを減らさない（読み方が変わる値は移行の手順を書く）。

## 関連

- [F-327](F-327-proxy-wasm-path-cost.md)（調査で見つけた）・[F-336](F-336-wasm-on-log-response-values.md)（`proxy_on_log` で見える値）・
  B-300（応答ヘッダの段のローカル応答）
