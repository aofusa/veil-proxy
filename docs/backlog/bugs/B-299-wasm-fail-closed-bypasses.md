# B-299: Proxy-Wasm のフィルタが拒否のつもりでも素通しになる経路（http_call の resume の trap・trap 前のローカルレスポンスの破棄・capability 不足の trap・インスタンス枠切れ）

**状態: 未修正（2026-11-05 起票。コードレビューで発見。行番号は f8dc80de）**

## 事象（コードの読み取り）

1. **http_call の応答を処理する間の trap が素通し**（メッシュの FAIL_CLOSED でも）: `src/wasm/engine.rs:702-710`・`:894-902` は resume の `Err` を
   `FilterAction::Continue` にするので、`:163-171` の fail closed の判定に届かない。文書が例に出している外部認可のパターンそのもの。
2. **trap の前に設定したローカルレスポンスを捨てる**: `local_response` は `Ok` の腕でだけ取り出す（`engine.rs:715-725`、本文の段は `:1301-1305`）。
   403 を返すと決めた後に trap すると素通しになる。Envoy はローカルレスポンスを即座に送る。
3. **capability が無いと黙ってフィルタが外れる**: capability が無いと host 関数はステータス 13 を返す（`host/headers.rs:209` 等、`host/stream.rs:52`）。
   proxy-wasm 0.2.5 の Rust SDK はそれを `unwrap` して panic → trap → fail open。capability の既定は全部 false なので、例えば
   `allow_send_local_response` を付け忘れた WAF は全要求を通す。
4. **インスタンス枠切れが素通し**: インスタンスの作成に失敗すると（`engine.rs:226-233` → `:163`）フィルタ無しで続く。再利用しないインスタンスは
   インラインの `http_call` を待つ間も枠を握るので、遅い呼び出しで `total_memories`（既定 128）を使い切らせると、後続の要求がフィルタを通らない。

## 改修案

1. resume の `Err` は `Err` として返し、ポリシー（[F-293](../features/F-293-wasm-failure-policy.md)）で扱う。
2. 設定済みのローカルレスポンスは、その後に trap してもポリシーに関係なく送る。
3. capability 不足のときに host 関数が返す値を見直す（SDK が panic しない `Status::NotAllowed` 相当か）、少なくともロード時に「このモジュールが import する
   host 関数に必要な capability が無い」を警告し、要求ごとのログは `log_throttled!`。文書に capability の付け忘れの症状を書く。
4. 枠切れは上限付きで空きを待ち、待てなければポリシーに従う（fail closed なら 503）。`veil_wasm_failures_total{reason="instantiate"}`。
- 試験: F-293 の単体・E2E に含める（各経路で 503・ローカルレスポンスが返ること、`total_memories = 1` で遅い http_call を 2 本）。

## 影響

- P1（セキュリティのフィルタの迂回）。1・2・4 は利用者の設定に関係なく起きる。

## 関連

- [F-293](../features/F-293-wasm-failure-policy.md)、[B-56](B-56-wasm-http-call-resume-epoch-expiry.md)
