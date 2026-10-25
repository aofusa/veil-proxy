# F-266: API ライブラリで multipart/form-data（ファイルのアップロード）とフォームを受ける（Rust・Python・C）

**状態: 完了（2026-10-25 起票・設計、同日実装。親は [F-258](F-258-v0100-plan.md)）**

## 背景

- Rust: `Form<T>` は `application/x-www-form-urlencoded` だけ（`serde_urlencoded`）。`multipart/form-data` の解析器は無い。
- Python: 本文は JSON だけを束縛する（`Form`・`File`・`UploadFile` の印が無い）。FastAPI の `UploadFile` を使うコードが動かない。
- 本文は既定で全部溜めてから呼ぶ（`[api] max_body_size` 10 MiB）。大きなファイルは `streaming = true`（Rust）・`stream_body=True`（Python）で受けるが、
  自分で境界を解析することになる。

## 制約

- ライブラリ利用時だけ（`src/api/`・`crates/veil-python`・`crates/veil-capi`）。本体は変わらない。
- ホットパス規則: 溜めた本文からは各パートを `Bytes` の切り出しで渡す（複製しない）。ストリーミングでは届いた断片を切り出して渡し、
  ファイルへ逃がすときの書き込みは `offload`（ブロッキングの書き込みをワーカーで行わない）。
- 依存を増やさない（multer は使わない。tokio・http の版に縛られ、ゼロコピーの方針とも合わない。解析器は小さいので自前で書く。境界の検索は既存の `memchr::memmem`）。

## 設計

### 1. 解析器（`src/api/multipart.rs`）

- RFC 7578 + RFC 2046 §5.1: `boundary`（1〜70 文字、引用符あり・なし）、前置き・後置きの無視、パートのヘッダ（`Content-Disposition`
  の `name`・`filename`・`filename*`（RFC 5987、UTF-8 のみ）、`Content-Type`、`Content-Transfer-Encoding` は無視）、
  終端 `--boundary--` の検査、CRLF 必須（LF だけは 400）。
- 2 つの形:
  - **溜めた本文**（`Bytes`）から `Multipart::parse(&body, boundary) -> impl Iterator<Item = Result<Part>>`。`Part::data()` は
    本文の `Bytes` の切り出し。
  - **ストリーミング**（`RequestBodyStream`）から `next_part().await` → `Part::chunk().await`（断片の `Bytes`）。境界が断片をまたぐ場合は
    境界の長さ分だけ持ち越す（持ち越しのための小さなバッファ 1 個。パートの本体は複製しない）。
- 上限（`[api]`、ライブラリ専用）: `multipart_max_parts`（既定 100）・`multipart_max_field_size`（ファイルでないパート、既定 1 MiB）・
  `multipart_max_file_size`（既定 = `max_body_size`）・`multipart_max_header_size`（パートのヘッダ、既定 8 KiB）。超えたら 413、
  書式の誤りは 400（FastAPI と同じ形の JSON の `detail`）。全体は今の `max_body_size`。
- ファイルの逃がし先: `UploadFile` は `multipart_spool_threshold`（既定 1 MiB）までメモリ、超えたら `upload_tmp_dir`
  （既定 OS の一時ディレクトリ）の名前の無い一時ファイル（Linux は `O_TMPFILE`、他は作って直ちに unlink）へ `offload` で書く。
  要求が終わったら閉じて消える。Landlock・unveil・capsicum を有効にしたライブラリ利用者のために、サンドボックスの書き込み許可に
  `upload_tmp_dir` を足す（`sandbox_paths` の `handler` の分岐）。

### 2. Rust の API

```rust
#[veil::route(path = "/upload", method = "POST", streaming = true)]
async fn upload(mut form: Multipart) -> Result<Json<Value>, ApiError> {
    while let Some(mut part) = form.next_part().await? {
        if part.file_name().is_some() { let file = part.save_to_temp().await?; /* file.path(), file.len() */ }
        else { let v = part.text().await?; }
    }
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize, Schema)]
struct Profile { name: String, age: u32 }
#[veil::route(path = "/profile", method = "POST")]
async fn profile(form: MultipartForm<Profile>, avatar: UploadFile) -> Response { /* 溜めた本文から */ }
```

- `Multipart`（ストリーミング。マクロは型名で見て `streaming = true` を自動で付ける）・`MultipartForm<T>`（ファイル以外のパートを
  serde で `T` へ。urlencoded と同じ規則で文字列を変換）・`UploadFile`（引数名 = パート名。`Vec<UploadFile>` で複数）。
  マクロが型名で抽出子を選ぶ作り（`crates/veil-macros/src/route.rs` 106〜126 行）に 3 個の名前を足す。
- `Form<T>` は `Content-Type` が multipart でも受ける（ファイルのパートは無視して 400 にしない。FastAPI の `Form` と同じ）。
- OpenAPI: `requestBody` に `multipart/form-data` のスキーマ（ファイルは `{"type": "string", "format": "binary"}`、`encoding` に `contentType`）。
  Swagger UI からファイルを選んで送れる。

### 3. Python の API（FastAPI と同じ名前）

```python
from veil import File, Form, UploadFile

@app.post("/upload")
async def upload(name: str = Form(...), files: list[UploadFile] = File(...)):
    return {"name": name, "sizes": [len(await f.read()) for f in files]}
```

- `Form()`・`File()`（既定値の印）と `UploadFile`（`filename`・`content_type`・`headers`・`size`・`file`（`SpooledTemporaryFile` 互換の
  ファイルオブジェクト）・`read`・`seek`・`close`、同期と `async` の両方）。`bytes = File()` も可。urlencoded の `Form()` も同じ印で受ける。
- 解析は Rust（上の解析器）で行い、Python に渡すのはパートの一覧（名前・ファイル名・型・`memoryview` か一時ファイルの fd）だけ
  （GIL の下で境界を探さない）。束縛は登録時に決める（今の `_build_params` と同じく要求ごとに `inspect` しない）。
- `python-multipart` には依存しない（FastAPI はそれを要求するが、veil は Rust の解析器を使う）。

### 4. C API

- `veil_multipart_next(req, veil_part_t *out)`（溜めた本文から。`name`・`filename`・`content_type`・`data`/`len` は本文の中を指す）。
  ストリーミングは `VEIL_ROUTE_STREAM_BODY` のルートで `veil_multipart_stream_*`。

## 試験

- 単体: RFC 7578 の例、境界の引用符、`filename*`、空のパート、境界が断片の境目に来る全位置（1 バイトずつ流す）、上限ごとの 413、
  終端の欠落・LF だけの 400。ファジング: `fuzz/` に解析器の対象を 1 個。
- E2E（Rust・Python・C、HTTP/1.1・h2c・HTTP/2・HTTP/3）: curl `-F` で小さなファイル・100 MB のファイル（ストリーミングで RSS が増えない
  ことを確かめる）・複数ファイル・フィールドとの混在、Swagger UI と同じ形の要求。一時ファイルが要求の後に残らない。
  Landlock を有効にしたライブラリで `upload_tmp_dir` に書ける。
- 互換: FastAPI の公式文書の「Request Files」の例が import の差し替えだけで動く。
- 性能: multipart でないルートは変わらない（解析器は抽出子を使ったときだけ動く）。100 MB のアップロードの CPU 時間を FastAPI + uvicorn と比べて記録。

## 文書

- `docs/guide/library.md`（英日）に「Forms and file uploads」の節、`crates/veil-python/docs/requests-responses.md`（英日）、
  C API のヘッダ、`examples/config.toml` の `[api]`。

## 受け入れ条件

- Rust・Python・C でフォームとファイルを受けられ、大きなファイルでメモリが増えない。本体のバイナリが変わらない。
- 文書（英日）・`backlog.md` が更新されている。

## 実施内容（2026-10-25）

- **解析器**（`src/api/multipart.rs`）: 溜めた本文とストリーミングで同じ同期の状態機械（`Core`: Start・AfterDelim・Headers・Data・
  DataEnd・Done）を使う。区切りは `\r\n--boundary` を `memchr::memmem::Finder` で探し、本体は届いた `Bytes` の切り出し。
  複製するのは断片をまたいだパートのヘッダと、次の断片に続き得る区切りの頭（`partial_suffix`）だけ。前置きは
  `max(multipart_max_header_size, 8 KiB)` まで、区切りの後の空白は 64 バイトまで。`filename*` は RFC 5987 の UTF-8 だけ。
  上限超えは 413、書式の誤りは 400（`{"detail": ...}`）。単体試験は RFC の例・全分割位置・上限・不正な本文・乱数の 20,000 本
  （溜めた結果とストリーミングの結果が一致すること）。ファジングの対象 `fuzz/fuzz_targets/multipart_parse.rs`（同じ一致を assert）。
- **Rust**: `MultipartForm<T>`（ファイル以外を `serde_urlencoded` の規則で `T`、`Schema` なら検証）・`UploadFile`・`Vec<UploadFile>`・
  `Option<UploadFile>`（引数名 = パート名、無ければ `loc = ["body", 名前]` の 422）・`Multipart`（`next_part` → `Field` の
  `chunk`・`bytes`・`text`・`save_to_temp`・`save_to`）。マクロは型名で選び、`Multipart` があればルートを `streaming = true` にし、
  溜める抽出子との併用はコンパイルエラー。`Form<T>` も multipart を受ける。OpenAPI はインラインの object（ファイルは
  `{"type": "string", "format": "binary"}`）。`[api] multipart_*`・`upload_tmp_dir`（`ApiSettings`）。
- **一時ファイル**: Linux は `O_TMPFILE`（`EOPNOTSUPP`・`EISDIR` のときだけ作って消す）、他の unix は作ってすぐ消す、Windows は
  `FILE_FLAG_DELETE_ON_CLOSE`。開く・書くのは `offload` の中。Landlock の書き込みパスと OpenBSD の unveil（rwc）に
  `upload_tmp_dir` を足した（`cfg(feature = "handler")`）。capsicum（FreeBSD の cap-mode）では一時ファイルを開けないので未対応
  （ライブラリの利用者は既定で cap-mode にしない）。seccomp は `unlink` を許していないので、`O_TMPFILE` の無いファイルシステムで
  seccomp を有効にすると後片付けが失敗する（Linux の既定の tmpfs・ext4・xfs・btrfs は `O_TMPFILE` がある）。
- **HTTP/1.1 の本文のストリーミング受信**（`src/proxy/h1.rs`）: 今まで HTTP/1.1 の `streaming_request` のルートも本文を溜めてから
  呼んでいた。`Content-Length` のある本文は、ハンドラの呼び出しと本文の汲み上げ（`pump_api_body`）を同じタスクの
  `FuturesUnordered` で並べて回す（F-173: `join!` を使わない）。ハンドラが本文を読み終える前に全量の応答を返したら、応答の後に接続を閉じる。
  ストリーミング応答を返したら（要求本文を応答へ流すエコー等）、本文を読み終えるまで応答の断片を溜めて要求本文のチャネルを空け続け、
  読み終えてから送る（同じ接続の読みと書きは同時にしない。最初の E2E で `/upper` の HTTP/1.1 が空の応答になって見つかった）。
  chunked の本文は今までどおり溜める。ストリーミング受信のハンドラのルートは HTTP/1.1・HTTP/3 でもルートの
  `max_request_body_size` と `[api] max_body_size` を見ない（HTTP/2 と同じ。パートごとの上限が効く）。HTTP/1.1 のルーティング前の
  10 MiB の検査（`pool::MAX_BODY_SIZE`）と `100 Continue` は、`handler` のビルドだけルーティングの後へ回した（本体のバイナリは変わらない）。
  HTTP/3 のストリーミング受信のハンドラの `max_request_body` も 0（無制限）にした。
- **Python**: `veil.Form()`・`veil.File()`・`veil.UploadFile`（`filename`・`content_type`・`headers`・`size`・`file`・`async` の
  `read`・`write`・`seek`・`close`・`read_bytes()`）。`Annotated[..., Form()]`・`list[...]`・`bytes = File()`・`alias=`。urlencoded も同じ
  `Form()`。Form・File の引数があるルートは登録時に `form=True`（`register_route`）でストリーミング受信にし、Rust の
  `multipart::prepare_form` がハンドラの前に本文を読む: テキストと `multipart_spool_threshold`（1 MiB）までのファイルはメモリ、
  超えるものは名前の無い一時ファイルの fd として `NativeRequest.multipart()` で渡す（Python が `os.fdopen` する。使われなかった fd は
  `Part.__del__` が閉じる）。GIL の下で境界を探さない。`python-multipart` に依存しない。ネイティブ無しの単体試験のために純 Python の
  小さな解析器を残した（サーバでは使わない）。`_veil.parse_multipart`・`spool_threshold` を足した。
- **C**: `veil_multipart_next(req, &part)`（`veil_part_t`: `name`・`filename`・`content_type`・`data`/`data_len` か一時ファイルの `fd`・`size`）、
  ルートのフラグ `VEIL_ROUTE_FORM`（コールバックの前に `prepare_form` で読み、大きなファイルは一時ファイル。`fd` はコールバックから
  戻ると閉じる）、`VEIL_ERR_FORM`（400）・`VEIL_ERR_TOO_LARGE`（413）。設計にあった `veil_multipart_stream_*`（C で境界を順に読む API）は
  作らず、`VEIL_ROUTE_FORM` の一時ファイルで大きなファイルを扱う形にした（C の呼び出し側が非同期の読み取りを組まずに済む）。
- **試験**: `tests/api/library_e2e.sh` の `run_rust_multipart`（rust-showcase: フォーム + ファイル・省略可能なファイル・検証の 422・ファイル
  無しの 422・複数ファイル・不正な本文の 400・h2c・ストリーミングの HTTP/1.1・h2c、100 MB を HTTP/1.1・h2c で送って常駐メモリの
  増加が 50 MiB 未満・一時ファイルが残らない・OpenAPI）、`run_python_multipart`（python-features: メモリと一時ファイルの両方を
  SHA-256 で照合、100 MB の HTTP/1.1・h2c、urlencoded、OpenAPI）、`run_c_multipart`（c-features: `VEIL_ROUTE_FORM` と溜めた本文、
  100 MB の HTTP/1.1・h2c、HTTP/3 で 30 MB）。HTTP/3 は curl が対応していないので試験クライアント（`tests/api/grpc_client`）に
  `--h3-fetch` を足した。`nonregression` に `veil::api::multipart`・`veil_multipart_` を足した。単体: `api::multipart` 10 件・
  C の `multipart_next_iterates_parts`・Python の `TestForms` 5 件。
- **文書**: `docs/guide/library.md`（英日）に「Forms and file uploads」の節と `[api] multipart_*`・`upload_tmp_dir`、
  `crates/veil-python/docs/requests-responses.md`、`crates/veil-capi/README.md`・`veil.h`、`examples/config.toml` の `[api]`。
- **残り**: Landlock を有効にしたライブラリでの E2E（`upload_tmp_dir` に書けること）と、FastAPI + uvicorn との 100 MB の CPU 時間の比較は
  していない。
