# F-273: `[route.traffic.cors]` を実際の応答にも効かせる（今はプリフライトだけ）

**状態: 完了（wt/core → feat/v0100、2026-10-25。2026-10-25 起票・設計。親は [F-258](F-258-v0100-plan.md)。[F-267](F-267-rust-kit-crate.md)・[F-268](F-268-python-kit-fastapi-compat.md) の CORS はこれを使う）**

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

## 実施内容（2026-10-25）

- **選び方**: `src/route_traffic.rs` の `CorsPolicy`。要求の `Origin` をルートの解決（`TrafficPolicy::select`）で 1 回だけ照合し
  （一覧の i 番目・`"*"`・許可しない・無い）、設定ロード時に作った `Backend` の写しを返す。写しは
  `SecurityConfig::cors_actual`（`#[serde(skip)]`、`src/route_cors.rs` の `CorsActual`）に足すヘッダを持つ。
  `[route.action]` の `resolved_backend` と分割先（`split`）ごとに、オリジンの数 + 2 個の写しを作る。`type = "Respond"` は
  `FixedResponse` を組み直してヘッダを応答に組み込む（HTTP/1.1 の応答全体も組み立て済み）。
- **足す場所**（`security.cors_actual` が `Some` のときだけ。無いルートは分岐 1 個）:
  - HTTP/1.1: 上流の応答の head（`compress.rs` の圧縮あり・なし・Wasm・失敗時、`https.rs`、`buffered.rs`、`forward.rs` の
    キャッシュの 304・メモリ・ディスク、HTTPS 上流の h2、h2c 上流）は `route_cors::apply_h1`（空行の前へ挿入。上流の
    `Access-Control-Allow-Origin` を照合）。veil が組み立てる head（MemoryFile・sendfile・Redirect）は `append_h1_lines`。
    ハンドラは `Response::headers` へ足す。kTLS の splice の中継は CORS のルートでは使わない（ユーザー空間の経路へ）。
  - HTTP/2: `build_h2_compressed_file_response`（MemoryFile・sendfile）・`h2_redirect`・`h2_api_respond`・
    `h2_emit_upstream_response`・`h2_relay_backend_response`（3 か所）・`h2_relay_h2_streaming` の応答ヘッダの配列。
  - HTTP/3: バッファ経路は `H3BufferedTask::send_msg` が `RespMsg::Head` に足す（ルートの解決の後に `cors` を入れる）。
    ストリーミング経路（Proxy・ハンドラ）は `BackendTaskParams::cors`・`H3HandlerStream::cors` を `ProxyStream::cors` に移し、
    メインループ（`streaming.rs`）が head を受け取ったときに足す。
- **設定**: `override_upstream`・`reject_disallowed` を新設（`expose_headers` は既存）。`reject_disallowed` はプリフライトにも効く。
- **レート制限**: 写しのセキュリティ設定は別のアドレスになるので、`SecurityConfig::rate_limit_zone`（`#[serde(skip)]`）に元の設定の
  アドレスを入れ、`check_security` はそれを鍵にする（オリジンごとに枠が分かれない）。
- **試験**: 単体 `route_cors::tests`（5 件: 空行の前への挿入・上流のヘッダの保持と上書き・`Vary` の追記）、`route_traffic::tests`
  （4 件追加: オリジンごとのヘッダ・`"*"`・`reject_disallowed`・レート制限の枠）。E2E `tests/standalone_e2e.rs` の
  `f273_cors_actual_responses_all_protocols`（HTTP/1.1・h2c・TLS の HTTP/2・HTTP/3 × Respond・File sendfile・File memory・
  Proxy（上流が付けない・付ける・`override_upstream`）・Redirect、許可しないオリジン、`reject_disallowed`、`"*"`、CORS の無いルート）。

### 決めたこと

- **`"*"` と `allow_credentials = true` の併用は今までどおり設定のエラーにした**（設計の「`*` でも credentials ならオリジンを
  そのまま」はしない）。オリジンを返すには要求ごとにヘッダの値を作る必要があり、組み立て済みのヘッダだけで済ませる方針と
  合わない。credentials を使うときはオリジンを列挙する（F-267・F-268 の CORS もそれに合わせる）。
- `Origin` の無い要求・許可しないオリジンの要求にも `Vary: Origin` を付ける（`"*"` だけの設定を除く）。応答がオリジンで
  変わるので、共有キャッシュに別のオリジン向けの応答を返させないため。
- `Vary` は既存の `Vary` を書き換えず、別の行（HTTP/2・HTTP/3 は別のフィールド）として足す（RFC 9110 で同じ意味）。
- 上流が `Access-Control-Allow-Origin` を返したら、`Vary` も含めて veil は何も足さない（上流の CORS の判断を尊重する）。
- ルートを選ぶ前に veil が返す応答（404・セキュリティの拒否・フォルトの中断・JWT のエラー）と WebSocket の Upgrade には付けない。
- ハンドラのルートの E2E は、`handler` feature が `full` に無いので standalone では回していない（単体の経路は同じ `apply_bytes`）。

### 残したこと

- ブラウザ相当の検査（Playwright の別オリジンの `fetch`）は F-264 の試験の仕組みができてから足す。
- 性能の交互 A/B（CORS の無いルートが変わらないこと・CORS のあるルートの費用）は F-276 の計測に回す。CORS の無いルートで
  増えたのは、各経路の `Option` の分岐 1 個と、HTTP/3 のバッファ経路の `Option<Arc>` の複製 1 回（`None`）だけ。
