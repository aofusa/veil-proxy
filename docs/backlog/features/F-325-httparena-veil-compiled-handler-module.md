# F-325: HttpArena の veil エントリで、Proxy-Wasm の代わりにハンドラを組み込む版を作る（今の veil の機能で h2o・nginx を超える案）

**状態: 未着手（2026-11-25 起票。F-317 の子。手元で作って測るまで。HttpArena への提出〈どちらの版を出すかを含む〉は人の確認の後〈F-269〉）**

## 背景

- infrastructure の規則は「ベンチマーク用に書いたハンドラのモジュールは許す。ただしサーバが読み込むモジュールであって、サーバの代わり
  ではないこと」「設定の制限は無い」。上位のインフラのエントリは全部、ハンドラをサーバに**組み込んで**いる:
  - okena: `make MODULES=httparena.inc` でハンドラをアセンブルして本体へ入れる（`route /baseline11 { handler baseline }`）。
  - agensio: CMake の `-DAGENSIO_HTTPARENA=ON` でハンドラ（`src/handlers/httparena.cpp`）を入れ、設定の `handler = "httparena"` で指す。
  - nginx: C のモジュール（`ngx_http_httparena_module.c`）を `--add-module` で入れ、`location / { httparena; }`。
  - h2o: libh2o に C のハンドラを直接登録したプログラム（`src/main.c` だけで設定ファイル無し。meta.json は `"type": "infrastructure"`）。
- veil は Proxy-Wasm のフィルタで答えていて、HttpArena では 1 要求に 60〜170 µs 上乗せされる（F-318）。F-318・F-319 と本体の改善
  （F-321〜F-324）を全部入れても、Proxy-Wasm のままでは約 3,500 点で h2o（3,874）に届かない見込み（F-317 の試算の S7）。
- veil には今、ハンドラを本体に組み込む機能がある（`handler`・`macro` フィーチャ。ハンドラはワーカーのイベントループの上で動き、
  `veil.toml` の `[[route]]` から `type = "Handler"`・`handler = "<名前>"` で指せる。`docs/guide/library.md`）。veil-api のエントリの値は
  この経路の値で、エントリをこの形にすれば試算は約 4,360 点（h2o を超える。S3）、F-322・F-323 を足すと約 4,960 点（nginx を超える。S5）。

## 作るもの（手元の写し `docs/artifacts/benchmark/entries/httparena/veil-module/`。git の管理外）

1. **ハンドラのモジュール**（小さな Rust のクレート）: `#[veil::route(..., name = "baseline")]` 等でハンドラを 3 つだけ定義する。
   - `baseline`（`/baseline11` の GET・POST、`/baseline2`）: クエリの `a`・`b` と（POST は）本文の整数の和を `text/plain` で返す。
   - `json`（`/json/{count}?m=`）: 起動時に `/data/dataset.json` を読んで型付きの値にし、要求ごとに手書きの直列化（今のフィルタと同じ
     やり方）でワーカーごとに使い回すバッファへ書く。
   - `main` はデータセットを読んで `veil::serve(Config::from_file("/etc/veil/veil.toml"))` を呼ぶだけ（ルートも TLS も設定ファイルから）。
2. **veil.toml**: 今の設定から `[wasm]` を外し、`/baseline11`・`/baseline2`・`/json/*` を `type = "Handler"` で名前を指す。`/static/*` は
   今の File バックエンド（F-320 の全体のキャッシュ・`precompressed`）、`/pipeline` は `Respond`（F-320）。セキュリティの設定は今のまま。
   ライブラリの既定はプロキシと違う（アクセスログ・OpenAPI が有効、seccomp・Landlock が無効。`docs/guide/library.md`）ので、
   `[access_log] enabled = false`・`[api] openapi_enabled = false`・`[security]` をプロキシのエントリと同じ値で明示する。
3. **Dockerfile**: crates.io の `veil-proxy`（`default-features = false`、`full-container` か既定の io_uring〈F-320 の計測で決める〉+ `macro`）
   でビルドする 2 段（veil-api のエントリと同じ形。`lto = "fat"`・`codegen-units = 1`）。手元の計測は作業ツリーへの `path` 依存で行う。
4. **README・meta.json**: 「ハンドラのモジュールを veil に組み込んでいる（okena・agensio・nginx と同じ形）。静的配信・TLS・HTTP/2・HTTP/3 は
   veil 本体と veil.toml」と書く。Proxy-Wasm の版（F-320）も同じ手元の場所に残す。

## 確かめること

- HttpArena の検証（`scripts/validate.sh` の veil の項目）と同じ要求を手元で通す（aofish の Docker で `validate.sh` も回す）。
- ノート PC（veil-load）と aofish（HttpArena の負荷ツールの Docker イメージ）で、Proxy-Wasm の版・組み込みの版・veil-api を同じ条件で測り、
  F-317 の試算を測った値で置き換える。

## 気を付けること（判断の材料）

- **区分の解釈**: 規則には「サーバを組み込んでアプリケーションのコードを載せたものは framework（openresty・ngx-php・frankenphp）」とも
  ある。ハンドラはベンチマークの 3 つの口だけで、サーバ・設定は veil のもの（h2o・agensio・okena と同じ）だが、HttpArena の保守者が
  veil-api（experimental）と同じものと見る可能性はある。提出の前に人が判断する。
- **見せたいもの**: 今のエントリは「Proxy-Wasm で拡張した veil」を見せている。組み込みの版はその代わりに順位を取りにいく。
  どちらを出すか（両方を残して説明する等）は提出のときに人が決める。この作業では両方を手元で用意して測るところまで行う。

## 受け入れ条件

- 手元で HttpArena の veil の検証項目が全部通る。ノート PC と aofish の計測の表（Proxy-Wasm の版との比較）を本チケットに書く。
- 提出はしない（fork・push・PR・コメントは人の確認の後）。

## 関連

- [F-317](F-317-httparena-infra-score-plan.md)（親）・[F-320](F-320-httparena-veil-entry-config.md)・[F-318](F-318-wasm-per-request-shared-writes.md)・
  [F-322](F-322-h1-pipelined-response-coalescing.md)・[F-323](F-323-h2-per-stream-fixed-cost.md)・F-188（API ライブラリ）・F-269
