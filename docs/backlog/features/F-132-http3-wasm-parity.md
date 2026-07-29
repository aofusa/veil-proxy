# F-132: HTTP/3 で WASM (Proxy-Wasm) を HTTP/1.1・HTTP/2 と同等のライフサイクルまで動かす

## 目的

HTTP/3 経路（`src/http3_server.rs`）の Proxy-Wasm 対応は HTTP/1.1・HTTP/2 経路（`src/proxy.rs`）に比べて
著しく手薄で、特に `on_log`（`proxy_on_log` / `on_request_complete_async`）が一度も呼ばれておらず、
WASM モジュール側のライフサイクル契約（Proxy-Wasm ABI v0.2.1）を満たしていなかった。
これを HTTP/1.1・HTTP/2 と同等の水準まで引き上げる。

## 現状の事実（対応前）

`docs/artifacts/f132_wasm_design.md` の調査結果より:

| フック | HTTP/1.1・HTTP/2（`src/proxy.rs`） | HTTP/3（`src/http3_server.rs`、対応前） |
|---|---|---|
| `on_request_headers` | あり | あり（`Backend::Proxy` バッファ経路のみ） |
| `on_response_headers` | あり | あり（`Backend::Proxy` のみ） |
| `LocalResponse` / `Pause` | あり | あり |
| `on_log`（`on_request_complete_async`） | 5 箇所で呼び出し | **0 箇所**（コンテキストが破棄されない） |
| `on_request_body` / `on_response_body` | 未配線（本チケットで H3 のみ配線） | 未配線 |
| `Backend::File`（静的配信）経路 | WASM 適用済み | **未適用** |

## 改修内容

1. **`on_log` の集約ヘルパ化**: `src/http3_server.rs` に `finish_h3_wasm_lifecycle()` を追加し、
   `handle_request` の **全離脱点**（LocalResponse による早期 return を含む）から呼ぶ。
   WASM モジュール未適用（`None` または空リスト）ならコストゼロで即 return する。
2. **`Backend::File`（`MemoryFile` / `SendFile`）経路への WASM 適用**:
   `on_request_headers` は既存の共通処理（全 `Backend` 種別で実行済み）のままとし、
   新たに `on_response_headers` を `apply_h3_wasm_response_headers` で適用。
   `handle_sendfile` にモジュールリストを引数追加。
3. **ボディフィルタの配線**（HTTP/3 の `Backend::Proxy` 経路。HTTP/3 は WASM 適用時に必ず
   `Decision::Buffer` に落ちるため、リクエスト・レスポンスとも本文全体がメモリ上にある）:
   - リクエスト: `on_request_headers` が `Continue` を返した直後に `on_request_body` を
     `end_of_stream=true` の1回呼びで適用。書き換えられた本文は上流へ転送する本文を差し替える。
   - レスポンス: `apply_h3_wasm_response_headers` の直後に `on_response_body` を適用。
     書き換えた場合は **content-length を新しい本文長へ更新**する（B-46 と同じ落とし穴:
     content-length の不一致・重複は nghttp3 が `H3_MESSAGE_ERROR` で切る）。
   - 共有ヘルパ `apply_wasm_request_body` / `apply_wasm_response_body` / `set_content_length_header`
     を `src/wasm/http_executor.rs` に追加（h1/h2 からも将来利用可能。今回配線したのは h3 のみ）。
4. **トレーラ（gRPC）は対象外**（F-133 で扱う）。

## 変更ファイル

- `src/http3_server.rs`: `finish_h3_wasm_lifecycle`、リクエスト/レスポンスボディフィルタ配線、
  `Backend::MemoryFile`/`handle_sendfile` への WASM レスポンスヘッダ適用。
- `src/wasm/http_executor.rs`: `apply_wasm_request_body` / `apply_wasm_response_body` /
  `set_content_length_header` / `WasmBodyOutcome` を追加。
- `src/wasm/tests.rs`: `f132_h3_lifecycle_tests`（on_log 後のコンテキスト非リーク回帰・
  ボディフィルタヘルパのパススルー・content-length 置換の単体テスト）。
- `tests/e2e_tests.rs`: HTTP/3 WASM E2E を追加
  （リクエストヘッダ変更・レスポンスヘッダ変更・LocalResponse・静的配信への適用）。
- `tests/e2e_setup.sh`: `waf_filter.wasm` の登録・`/waf/*`（LocalResponse 検証用）・
  `/wasm-static`（`Backend::File` への WASM 適用検証用）ルートを追加。
- `tests/fixtures/wasm/waf_filter.wasm`: `tests/wasm/waf_filter.wasm`（gitignore 対象）の
  追跡済みフォールバックコピーを追加（`header_filter.wasm` 等の既存パターンに合わせる）。

## 既知の限界・注意点

- `src/wasm/persistent_context.rs` の `store_context`/`take_context` は現行の
  リクエスト処理経路（h1/h2/h3 いずれも）から実際には呼ばれておらず、
  `get_context_stats()` は通常のリクエスト処理では常に 0 を返す
  （async HTTP call の pause/resume 専用の別経路が実装済みになった時点で使われる想定）。
  そのため「on_log 後にコンテキストが解放される」ことの E2E からの直接観測は
  意味を持たず、`src/wasm/tests.rs::f132_h3_lifecycle_tests` で
  リクエストヘッダ処理 → `on_log` の往復を繰り返してもコンテキスト数が
  単調増加しないことを回帰テストとして固定した。
- `on_response_headers` が `LocalResponse` / `Pause` を返した場合、
  HTTP/3 経路（`apply_h3_wasm_response_headers`）は h1/h2 と同様に
  無視して元のヘッダで継続する（既存動作を変更していない）。

## テスト

- `cargo build --features full`
- `cargo test --lib --features full wasm`
- `./tests/e2e_setup.sh test`
- `cargo clippy --features full --all-targets -- -D warnings`
- `cargo fmt --all`

## 受け入れ条件

- [x] HTTP/3 経路のすべての離脱点から `on_log` が呼ばれ、コンテキストリークの回帰テストがある。
- [x] `Backend::File`（`MemoryFile`/`SendFile`）にも WASM レスポンスヘッダフィルタが適用される。
- [x] HTTP/3 の `Backend::Proxy` 経路でリクエスト/レスポンスボディフィルタが動作し、
      レスポンス本文書き換え時に content-length が正しく更新される。
- [x] HTTP/1.1・HTTP/2 経路（`src/proxy.rs`）の既存挙動を変更していない。
- [x] `docs/backlog/backlog.md` に本チケットを追加。
