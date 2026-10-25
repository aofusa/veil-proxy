# F-266: API ライブラリで multipart/form-data（ファイルのアップロード）とフォームを受ける（Rust・Python・C）

**状態: 未着手（2026-10-25 起票・設計。親は [F-258](F-258-v0100-plan.md)）**

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
