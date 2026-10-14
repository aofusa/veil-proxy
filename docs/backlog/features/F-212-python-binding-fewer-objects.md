# F-212: Python バインディングの要求ごとのオブジェクト生成を減らす（使い勝手は変えない）

**状態: 完了（feat/api-roadmap-impl、2026-10-14）**

## 背景（F-201 の計測）

`tools/api-bench` の Python（2 スレッド、`offload=True`）は plaintext 15,467 req/s・json 11,726 req/s、
CPU 106.4 µs/req。HTTP の解析・ルーティング・応答は Rust で、Python で動くのはハンドラ本体だけだが、
Rust と Python の境目で要求ごとに多数のオブジェクトを作っている。GIL で直列化されるので、Python 側の 1 要求あたりの
仕事がそのままプロセス全体のスループットの上限になる。

## 現状: 1 要求あたりに作るもの（`crates/veil-python/src/lib.rs` の `call_python`・`python/veil/__init__.py`）

| 段階 | 作るもの |
|---|---|
| Rust → Python の引数 | ヘッダ N 個 × (`String` 2 個（`from_utf8_lossy(..).into_owned()`）→ `str` 2 個 + `tuple`)、`list`、`params` の `dict`、`method`・`path`・`query`・`client_ip` の `str`（`client_ip` は Rust の `String` も）、本文の `bytes`（複製）、`handler_id` の `int`、8 要素の引数 `tuple` |
| `_dispatch` | `Request`（インスタンスの `__dict__`）、`Headers` と、ヘッダの `tuple` N 個と `list` を**もう一度**（`[(str(k), str(v)) ...]`）、`dict(params)` の複製 |
| `Route.bind` | `kwargs` の `dict`・`errors` の `list`（引数が無いハンドラでも） |
| 戻り値 | `Response`（と `JSONResponse`）・`headers` の `list` と `tuple`・`_triple` の `tuple`、`str` の応答は `encode` した `bytes`、`bytes(body)` の複製 |
| Python → Rust | `(u16, Vec<(String, String)>, Vec<u8>)` への取り出し（ヘッダごとに `String` 2 個、本文の `Vec` への複製）、`Bytes` 化 |
| `async def` | 要求ごとに `asyncio.run`（イベントループを毎回作って閉じる） |

## 方針

- 利用者から見える API は変えない: `req.method`・`req.path`・`req.query`・`req.headers`（大文字小文字を区別しない
  読み取り専用の辞書。`get`・`getlist`・`[]`・`in`・`keys`・`items`・`len`・反復）・`req.body`（`bytes`）・`req.params`（`dict`）・
  `req.client_ip`・`req.query_params`・`req.json()`・`req.text()`、戻り値の変換規則、`Response`・`JSONResponse`・
  `HTTPException`、デコレータ、引数の束縛、pydantic、OpenAPI。`crates/veil-python/tests/test_binding.py`（32 件）・
  `tests/e2e.sh`・`library_e2e.sh` の Python 節・`examples/api/python-*` を変えずに通す。
- 作るのは「使われたものだけ」（遅延生成とキャッシュ）。ハンドラが見ないヘッダ・本文・パラメータは Python のオブジェクトにしない。
- 新しい必須依存を足さない。

## 設計

### 1. `Request` を Rust のクラスにする（遅延生成）

- `veil._veil.Request`（`#[pyclass]`）が Rust の `veil::api::Request`（`Bytes` で持つ）をそのまま包む。各属性は `#[getter]` で、
  初回アクセス時に Python のオブジェクトを作ってキャッシュする:
  - `method`: よく使うメソッド（GET・POST・PUT・DELETE・PATCH・HEAD・OPTIONS）は `pyo3::intern!` の文字列を返す（生成無し）。
  - `path`・`query`・`client_ip`: 初回に `str`。
  - `body`: 初回に `bytes`（複製は 1 回。今は Rust → `bytes` → `Request` の 2 段）。
  - `headers`: Rust のクラス `veil._veil.Headers`。`get`・`getlist`・`[]`・`in` は Rust のヘッダ配列を直接照合して、
    返す値の `str` だけを作る。`items()`・`keys()`・反復のときだけ一覧を作る。
  - `params`: 初回に `dict`。
  - `query_params`・`json()`・`text()` は今と同じ意味で Rust 側に置く（`query_params` はキャッシュ）。
- 単体試験のために `Request(method, path, query, headers, body, params, client_ip)` で作れるようにする（`#[new]`、今の
  Python の `Request` と同じ引数）。`isinstance(x, veil.Request)`・型注釈 `req: veil.Request` もこのクラスを指す。
- 今の Python の `Request` には任意の属性を足せる（`req.user = ...` 等）。互換のため `#[pyclass(dict)]` にする（`__dict__` は
  属性を足したときだけ作られる）。pyo3 の abi3 で `dict` を使うには Python 3.9 以上が要るので、`abi3-py38` を
  **`abi3-py39` に上げる**（Python 3.8 は 2024-10 にサポート終了）。README とガイドの対応版を直す。

### 2. 呼び出しの経路を短くする

- `run()` の時点で `handler_id` ごとに `Route` の呼び出し口（下の 3 で作る関数）を Rust 側に `Py<PyAny>` で持ち、
  要求ごとに `_dispatch`（`_HANDLERS[handler_id]` の引き当て・`Request` の組み立て・8 要素の `tuple`）を通さず、
  1 引数で直接呼ぶ（`call1((req,))`）。

### 3. 引数の束縛をルートごとに前もって決める

`Route` の登録時に、引数の種類から専用の関数を選ぶ:

| ハンドラの引数 | 呼び方 |
|---|---|
| 無し | `func()`（`kwargs`・`errors` を作らない） |
| `req` だけ | `func(req)` |
| パスの引数だけ（型変換無し・`str`） | `func(**req.params)` 相当を位置引数で |
| それ以外 | 今の `bind`（検証エラーの形も今と同じ） |

`errors` の `list` はエラーが出たときだけ作る。

### 4. 戻り値を Rust で受ける

- ハンドラの戻り値の変換（`to_response`）のうち、よくある形は Rust 側で直接受ける:
  - `str` → UTF-8 のバイト列をそのまま本文に（`encode` の `bytes` を作らない）、`content-type: text/plain; charset=utf-8` は
    Rust の静的な `Bytes`。
  - `bytes` → `PyBackedBytes` を `Bytes::from_owner` で包む（複製しない。`bytes` 1.12 は `from_owner` を持つ）。
  - `dict`・`list` → 今と同じ `_dumps`（`json.dumps` の同じ引数）を呼んだ `bytes` を同様に複製無しで本文に。
    （Rust 側で JSON に直す案は、`default=_json_default`・`ensure_ascii=False`・浮動小数の表記まで今と同じにする必要があり、
    効果を測ってから判断する。）
  - `None` → 204、`(body, status)`・`(body, status, headers)` → 同じ規則。
  - `Response` → 属性を直接読む（`_triple` の `tuple` を作らない）。`headers` は既知の名前（`content-type` 等）を静的な
    `Bytes` に寄せ、値だけを複製する。
- 上に当てはまらないもの（pydantic のモデル等）は今の `to_response` に回す。

### 5. `async def` のイベントループを使い回す

- プールのスレッドごとに 1 個のイベントループ（`asyncio.new_event_loop()`）を作って `run_until_complete` で使う。
  `asyncio.run` が毎回作るループ・セレクタ・シグナル処理を作らない（F-210 の非同期ハンドラ・非同期ジェネレータもこれを使う）。
- `offload=False`（ワーカー上）のときも同じく、ワーカーのスレッドのループを使う。

### 6. 計測を足す

- `tools/api-bench/python` に「Python 側の確保回数 / 要求」を足す: `PYTHONMALLOC=malloc` で動かし、malloc・calloc・realloc
  を数える小さな `LD_PRELOAD` の共有ライブラリ（`tools/api-bench/python/malloc_count.c`、`SIGUSR1` で累計を出す）で
  wrk の前後の差を要求数で割る（Rust 側の確保も合わせて数える。veil-python はグローバルアロケータを定義しないので
  システムの malloc を通る）。
- ハンドラの種類を増やす: 引数無しの `str`、パス引数 + `dict` を返す json、`req.headers.get`、pydantic の本文、`async def`。

## 手順

1 → 2 → 3 → 4 → 5 の順に 1 コミットずつ、各段で `tools/api-bench/run_bench.py --group python`（`BENCH_BASE_DIR` の交互 A/B）と
確保回数を記録する。各段で `test_binding.py`・`e2e.sh`・`library_e2e.sh python` を通す。

## 受け入れ条件

- Python の plaintext・json の CPU µs/req を **30% 以上下げる**（106.4 → 75 以下）。req/s と p99 を記録し、確保回数 / 要求の
  前後を `docs/perf/README.md` に載せる。`async def` の行も載せる。
- 既存の Python の試験とサンプルが変更無しで通る（使い勝手が変わらないことの確認）。変わるのは対応する Python の下限
  （3.8 → 3.9）だけで、ガイド（英日）と `crates/veil-python/README.md` に書く。
- Linux に加えて macOS・Windows（F-203 の手順）で Python の節を 1 回。

## 関連

F-194（Python バインディング）、F-201（ベンチ）、F-210（C・Python の機能拡充。ストリーミングの `Request` もこのクラスの上に作る）。

## 実施内容（2026-10-14）

- `veil._veil.NativeRequest`・`NativeHeaders`（`crates/veil-python/src/native.rs`）: Rust の `Request` を包み、属性は初回アクセスで
  作って `GILOnceCell` にキャッシュ。`method` はよく使うものを intern。`__dict__` を持つ（任意の属性を足せる）。
  `veil.Request`・`veil.Headers` を `abc.ABCMeta` にして仮想サブクラスとして登録（`isinstance` が真）。
- **純 Python の `Request` と `Route.handle` は残した**（`test_binding.py` が拡張無しで動く前提を保つ）。サーバだけが
  `Route._call`（ルートの登録時に、引数無し・`req` だけ・`str` のパス引数だけ・それ以外（`bind`）から選んだ呼び出し口）を
  `NativeRequest` 1 つで呼ぶ。`_dispatch`・8 要素の引数タプル・Python の `Request`/`Headers` の生成が無くなった。
- 戻り値は Rust で応答にする: `str`（UTF-8 のまま）・`bytes`（`PyBackedBytes` で複製無し）・`dict`/`list` は **Rust で JSON に**
  （`str`・64 ビット整数・真偽値・None・dict・list・tuple だけのとき。`_dumps` と同じバイト列であることを試験で確認。
  float・str 以外のキー・巨大な整数・孤立サロゲートは `_dumps` に戻す）・`None`・`veil.Response`。それ以外は `Route.to_response`。
- `async def` はスレッドごとに 1 つのイベントループ（`_run_coro`。`Route.handle` も同じ）。
- 設計の「`Response` のヘッダを既知の名前で静的にする」は `known_header` で一部。Python の下限は 3.9（abi3-py39、`dict` の
  ため）。
- 試験: `tests/test_native.py`（新規、12 件: 属性・ヘッダが純 Python 版と一致・キャッシュ・isinstance・追加属性・`_call` と
  `handle` が同じ応答（引数の種類 7 通り・戻り値の型 7 通り）・Rust の JSON 化が `_dumps` と同じバイト列）。既存の
  `test_binding.py`（32 件）・`e2e.sh`・`library_e2e.sh python` は無修正で通る。

### 計測（`tools/api-bench`、2 スレッド・64 接続・5 ラウンド交互、中央値。`docs/artifacts/f212_ab_py.tsv`）

| 対象 | plaintext req/s | plaintext CPU µs/req | json req/s | json CPU µs/req |
|---|---:|---:|---:|---:|
| 改修前 offload=True | 15,848 | 110.2 | 12,237 | 140.5 |
| **改修後 offload=True** | **25,494（+61%）** | **71.1（−35%）** | **22,567（+84%）** | **83.9（−40%）** |
| 改修前 offload=False | 16,823 | 107.5 | 13,308 | 122.9 |
| 改修後 offload=False | 26,229（+56%） | 77.0（−28%） | 24,226（+82%） | 83.3（−32%） |
