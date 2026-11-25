# F-325: HttpArena の veil エントリで、Proxy-Wasm の代わりにハンドラを組み込む版を作る（今の veil の機能で h2o・nginx を超える案）

**状態: 完了（2026-11-26、手元で作って検証・計測した。HttpArena の validate.sh は 72 件すべて通過、試算は約 4,770 点〈4 位〉。HttpArena への提出〈どちらの版を出すかを含む〉は人の確認の後〈F-269〉。F-317 の子）**

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

## 進捗

### 1. 作ったもの（2026-11-26。`docs/artifacts/benchmark/entries/httparena/veil-module/`、git の管理外）

- `src/main.rs`: `#[veil::route(path = ..., method = ...)]` のハンドラ 4 つ（`/baseline11` の GET・POST、`/baseline2`、
  `/json/{count}`）。ルートは `veil.toml` の `type = "Handler"` で名前を指す形ではなく、マクロの `path` でライブラリのルートとして
  入れる（`veil.toml` に書くのは `/static/*`・`/pipeline` だけ）。データセットは起動時に 1 回読んで型付きの行にし、文字列は JSON の
  エスケープ済みの形で持つ。`/json` は要求ごとにフィールドを書いて直列化する（`total` は `m` で変わる。要求の間で何も使い回さない）。
  グローバルアロケータは veil-alloc（ライブラリは `#[global_allocator]` を定義しないので、アプリで指定する）。
- `veil.toml`: Proxy-Wasm の版（F-320）から `[wasm]` を外し、`[api] openapi_enabled = false` を足した。リスナー・TLS・`[security]`
  （nonroot への降格・seccomp・Landlock）・静的配信の設定は Proxy-Wasm の版と同じ。
- `Dockerfile`（提出用。crates.io の次の版を `features = ["full", "macro"]`〈io_uring〉でビルドする 2 段。`Cargo.lock` は提出のときに
  作る）、`Dockerfile.local`（手元の検証用。作業ツリーでビルドしたバイナリを入れる）、`meta.json`、`README.md`。

### 2. aofish の計測（86297922、交互 3 巡の中央値。rps / 1 要求の CPU µs）

F-317 の「計測の記録」と同じ条件（サーバは CPU 0-7、負荷ツールは 8-15、1 回 5 秒）。wrk の TLS で WSL2 の時計が戻った回（エラーが
接続の数ほど出て rps が水増しされる。B-357）は除いた。

| 負荷 | v0.10.1 の今のエントリ（epoll） | Proxy-Wasm の版（epoll） | Proxy-Wasm の版（io_uring） | 組み込みの版（epoll） | 組み込みの版（io_uring） | veil-api の設定（epoll） |
|---|---|---|---|---|---|---|
| h1（`/baseline11`、512 接続） | 129k / 55.72 | 283k / 25.38 | 436k / 16.48 | 560k / 12.88 | 655k / 10.76 | 571k / 12.63 |
| パイプライン（`/pipeline`、256 × 16） | 202k / 35.53 | 1.49M / 3.31 | 1.56M / 3.44 | 1.47M / 3.26 | 1.52M / 3.28 | 1.56M / 3.62 |
| h2c（`/baseline2`、64 × 100） | 359k / 20.30 | 1.23M / 5.93 | 1.27M / 5.84 | 2.77M / 2.64 | 2.79M / 2.62 | 2.72M / 2.69 |
| h2（TLS、64 × 100） | 352k / 20.78 | 1.03M / 7.19 | 1.05M / 7.06 | 2.12M / 3.41 | 2.08M / 3.52 | 2.12M / 3.45 |
| h3（16 × 64） | 95k / 9.70 | 130k / 7.25 | 116k / 7.97 | 170k / 5.30 | 179k / 5.10 | 164k / 5.43 |
| json-tls（wrk 256） | 110k / 66.09 | 198k / 36.48 | 193k / 37.37 | 368k / 18.15 | 374k / 18.24 | 299k / 24.01 |
| static-tls（wrk 256） | 87k / 65.56 | 213k / 30.58 | 222k / 30.34 | 209k / 30.66 | 216k / 30.38 | 214k / 28.48 |
| static-h2（64 × 32） | 5k / 1,139 | 392k / 18.91 | 376k / 19.36 | 395k / 18.42 | 393k / 18.68 | 373k / 19.42 |
| static-h3（16 × 64） | 1k / 735 | 35k / 26.44 | 32k / 28.66 | 38k / 24.38 | 34k / 26.55 | 34k / 26.62 |

- 組み込みの版は Proxy-Wasm の版より h1 +50%（io_uring）、h2c・h2 約 2 倍、h3 +54%、json-tls 約 1.9 倍。静的配信と `/pipeline` は
  どちらも本体の経路なので同じ。veil-api の設定（同じ木）とは同等で、json-tls は +25%（直列化の違い）。
- io_uring と epoll: h1 はどちらの版も io_uring が速い（組み込み +17%・Proxy-Wasm +54%）。static-h3 は epoll が速い（組み込み
  −10%・Proxy-Wasm −8%）。h3 は組み込みは io_uring +5%、Proxy-Wasm は epoll +12%。他は ±4% の中。下の試算の点ではどちらの版も
  io_uring が上（+240・+320）なので、どちらのエントリも io_uring（既定のビルド）にする（F-320 の 5）。
- ノート PC の計測は行わなかった。作業の間、ノート PC は別の作業のビルド・試験で負荷が 7〜9（4 CPU）あり、比較にならない。
  専有のロックを取って測れる aofish の値だけを使う。

### 3. 試算（F-317 の試算を測った値で置き換える。`docs/artifacts/benchmark/httparena-perf/scripts/scen2.py`）

HttpArena の veil-api の値（v0.10.1、2026-11-24）に、aofish で測った「その版 / 同じ木の veil-api の設定」の比をプロファイルごとに
掛けた（baseline・limited-conn は h1、pipelined はパイプライン、baseline-h2 は h2、baseline-h3 は h3 の比）。HttpArena の veil-api は
v0.10.1 なので、その後の本体の改善（F-321・F-323・B-501 等）は入らない（低めの見積もり）。

| エントリ | 点 | 順位（9 エントリ） |
|---|---|---|
| 組み込みの版（io_uring） | 約 4,770 | 4（fit 4,521・h2o 3,874 を超える。nginx 4,779 の約 10 点下） |
| 組み込みの版（epoll） | 約 4,530 | 4 |
| Proxy-Wasm の版（F-320、io_uring） | 約 3,580 | 6（h2o に届かない） |
| Proxy-Wasm の版（F-320、epoll） | 約 3,260 | 6 |

- 組み込みの版の内訳（点）: baseline 724・pipelined 123・limited-conn 826・json-tls 788・static-tls 761・baseline-h2 315・static-h2 620・
  baseline-h3 328・static-h3 284。最も低いのは pipelined（推定 3.3M、最高は 27M）と baseline-h2（5.2M、最高は 16.5M）。
- v0.10.1 のエントリを基準にした比（`v0101-epoll@entry`）で出すと低く出る（組み込みの版 3,530・Proxy-Wasm の版 2,590）。v0.10.1 の
  エントリは 64 コアで伸びない（F-318 の全ワーカー共有の書き込み、B-495 で静的配信が 11〜19 コアしか使えない）が、8 コアの aofish では
  その差が出ないため、この基準は 64 コアの伸びを過小に見る。

### 4. HttpArena の検証（validate.sh）

- aofish の Docker Desktop で HttpArena（3388e45）の `scripts/validate.sh` を `VALIDATE_SKIP_BUILD=1` で回した。イメージは
  `Dockerfile.local`（ubuntu:24.04 に、86297922 でビルドした io_uring のバイナリと `veil.toml` を入れたもの）。
  **72 件すべて通過（失敗 0・スキップ 0）**: HTTP/1.1・パイプライン・TLS・h2・h2c・HTTP/3（h2load-h3 で QUIC）・静的ファイルの
  差し替えへの追従・127.0.0.160 からの接続・TLS の判定。
- 手元の写しの validate.sh は HTTP/3 の検査 2 つの URL を `localhost` から `127.0.0.1` に替えて回した（Docker Desktop の VM では
  `localhost` が先に `::1` になり、公開した UDP のポートは IPv4 だけのため。TCP の検査は `localhost` のまま）。

## 受け入れ条件

- 手元で HttpArena の veil の検証項目が全部通る。ノート PC と aofish の計測の表（Proxy-Wasm の版との比較）を本チケットに書く。
- 提出はしない（fork・push・PR・コメントは人の確認の後）。

## 関連

- [F-317](F-317-httparena-infra-score-plan.md)（親）・[F-320](F-320-httparena-veil-entry-config.md)・[F-318](F-318-wasm-per-request-shared-writes.md)・
  [F-322](F-322-h1-pipelined-response-coalescing.md)・[F-323](F-323-h2-per-stream-fixed-cost.md)・F-188（API ライブラリ）・F-269
