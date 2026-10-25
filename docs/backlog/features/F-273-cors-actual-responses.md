# F-273: `[route.traffic.cors]` を実際の応答にも効かせる（今はプリフライトだけ）

**状態: 未着手（2026-10-25 起票・設計。親は [F-258](F-258-v0100-plan.md)。[F-267](F-267-rust-kit-crate.md)・[F-268](F-268-python-kit-fastapi-compat.md) の CORS はこれを使う）**

## 背景（調査で確認）

- `[route.traffic.cors]`（`src/route_traffic.rs` 298〜303 行 `CorsPreflight`）は、`OPTIONS` + `Access-Control-Request-Method` の
  プリフライトに設定ロード時に作った固定の応答を返すだけ。実際の応答（GET・POST 等）に `Access-Control-Allow-Origin` を足す経路は
  どのバックエンドにも無い（文書 `docs/guide/routing.md` 867・934 行も「CORS preflight」と書いている）。
- 上流が CORS のヘッダを付けないプロキシのルート、File のルート、ハンドラのルートでは、プリフライトが通っても本番の要求を
  ブラウザが拒否する。

## 制約

- 本体の機能。`[route.traffic.cors]` を書いていないルートは 1 行も変わらない。
- ホットパス: 足すヘッダは設定ロード時に組み立てた `Bytes`（`from_static` か `clone`＝参照カウント）。要求ごとの確保は無い。
  `Origin` の照合は今のプリフライトと同じ完全一致の表（`*` のときは照合しない）。

## 設計

- 実際の応答（プリフライト以外）で、要求に `Origin` があり許可されたオリジンなら、応答ヘッダに次を足す:
  `Access-Control-Allow-Origin`（許可したオリジン、`*` の設定でも `allow_credentials = true` ならオリジンをそのまま）・
  `Access-Control-Allow-Credentials: true`（設定したとき）・`Access-Control-Expose-Headers`（新設キー `expose_headers`）・
  `Vary: Origin`（`*` 以外のとき。既存の `Vary` に足す）。
- 上流（プロキシのルート）が既に `Access-Control-Allow-Origin` を返したら上書きしない（`override_upstream = true` で上書き）。
- 適用する場所: 3 プロトコルの応答ヘッダを書く共通の点（HTTP/1.1 は `h1.rs` の応答ヘッダの組み立て、HTTP/2・HTTP/3 は応答の
  ヘッダの配列）。ルートの解決で決まった CORS の設定（`Option<Arc<..>>`）と、要求の `Origin` の照合結果を応答まで持ち回る
  （照合はヘッダの解析時に 1 回）。File・Proxy・Handler・Redirect・Respond の全部。
- 許可しないオリジンの実際の要求は今どおり処理して、CORS のヘッダを付けないだけ（ブラウザが拒否する。サーバ側で 403 にしない。
  仕様どおり。`reject_disallowed = true` で 403 にもできる）。

## 試験

- 単体: 許可・不許可・`*`・credentials・`Vary` の追記・上流のヘッダの扱い。
- E2E（HTTP/1.1・h2c・HTTP/2・HTTP/3、Proxy・File・Handler）: 実際の GET に `Access-Control-Allow-Origin` が付く、ブラウザ相当の
  検査（Playwright で別オリジンの `fetch`。F-264 の試験の仕組みを共有）。
- 性能: CORS の無いルートの交互 A/B（変わらない）、CORS のあるルートの費用を記録。

## 文書

- `docs/guide/routing.md`（英日）の CORS の節を「プリフライトと実際の応答」に書き直す。`examples/config.toml`。移行ガイド（nginx の
  `add_header Access-Control-*`・Envoy の CORS フィルタ）の対応表を更新。

## 受け入れ条件

- 全バックエンド・3 プロトコルで実際の応答に CORS のヘッダが付き、CORS の無いルートの性能が変わらない。文書（英日）・`backlog.md` が更新されている。
