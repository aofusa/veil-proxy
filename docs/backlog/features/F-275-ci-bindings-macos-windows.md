# F-275: Python の wheel と C のライブラリを macOS・Windows でも CI で作って試す

**状態: 完了（wt/core → feat/v0100、2026-10-25。GitHub Actions での初回の実行は rc の時。2026-10-25 起票・設計。親は [F-258](F-258-v0100-plan.md)）**

## 背景（調査で確認）

- Python の wheel の macOS（arm64・x86_64）・Windows（x86_64）のビルドと試験は `release.yml` の `wheels` の job だけ
  （253〜330 行）。普段の `ci.yml` は Linux の wheel だけ（文書と stubtest、139〜165 行）。Windows arm64 の wheel は無い。
- `veil-capi` は **どの OS の CI でもビルドしていない**（Linux の `cargo build` のワークスペースのメンバーとしてだけ）。
  C のライブラリは `packaging/scripts/build-lib.sh`・`docker/Dockerfile.lib`（Linux manylinux_2_28）で作る。
- v0.10.0 で足す WebSocket・常駐のイベントループ（Windows・macOS は eventfd が無いので自分宛てのソケットの組）・multipart の
  一時ファイル・ASGI・ファイルの変更通知（`ReadDirectoryChangesW`・kqueue）は OS の差が出やすい。リリースの時に初めて壊れていると分かると、
  リリースの手戻りが大きい（v0.9.0 の draft が作れなかった件と同じ形）。

## 設計

- `ci.yml` に job `bindings`（push・PR、`paths` で `src/api/**`・`crates/**`・`src/runtime/**` 等に限る）を足す:
  | runner | 内容 |
  |---|---|
  | `macos-14`（arm64） | `maturin build` → `pip install` → `crates/veil-python/tests`（純 Python + ネイティブ）→ E2E の要点（`tests/api/library_e2e.sh python` の最小版: HTTP/1.1・h2c・WebSocket・multipart・async）。`cargo build -p veil-capi` → `examples/api/c-hello` をビルドして 1 要求 |
  | `windows-2022`（x86_64） | 同じ（PowerShell 版の最小 E2E、`tests/api/` に `.ps1` を足す。F-203 の Windows の手順を流用） |
  | `ubuntu-24.04` | `veil-capi` のビルドと C の例（今は無い） |
- 重い試験（Autobahn・Playwright の Swagger UI のオフライン・FastAPI 互換の全件）は `examples-nightly.yml` に足す（毎晩、Linux）。
- Windows arm64 の wheel（`windows-11-arm` の runner が使えるなら）を `release.yml` に足すかは、runner の可用性を見て決める（使えなければ記録だけ）。
- 所要時間の目安を測り、PR の待ち時間が増えすぎるなら `paths` と並列で抑える（目標: 既存の最長の job を超えない）。

## 受け入れ条件

- macOS・Windows の bindings の job が緑で、v0.10.0 の新機能の最小 E2E が含まれる。`docs/guide/testing.md`（英日）に CI の表を更新。`backlog.md` が更新されている。

## 実施内容（2026-10-25）

- `.github/workflows/bindings.yml`（新設）: job `bindings` を macOS（`macos-14`、arm64）・Windows（`windows-2022`、x86_64）・
  Linux（`ubuntu-24.04`）の matrix で回す。中身は `tests/api/library_e2e.sh` の `c`・`python`（Linux は `c` だけ。Linux の
  wheel は `ci.yml` の `python-docs` が作る）。F-203 で各 OS で回せるようにした台本をそのまま使うので、CI と手元・検証機の
  手順が同じになる。
  - `c`: `cargo build -p veil-capi --release` → `examples/api/c-hello`（動的・静的リンク。Windows は MSVC の `cl`）を起動して
    HTTP/1.1・h2c・OpenAPI・アクセスログ、`examples/api/c-features`（ストリーミング・L4・gRPC の相互接続 h2c / HTTP/3）。
  - `python`: `maturin develop --release` の venv で単体（`test_binding.py`・`test_native.py`）・`tests/e2e.sh`（HTTP/1.1・h2c）・
    ストリーミング・async・L4・gRPC（基本の層・grpcio の層）・GraphQL。
  - 依存: macOS は `brew install protobuf openssl@3`（LibreSSL の `openssl req` は rustls が受け付けない v1 の証明書を作る）、
    Windows は `choco install nasm curl protoc`（HTTP/2 の無い Git for Windows の curl の前に curl.se のビルドを置く）、Linux は
    `cmake nasm clang protobuf-compiler`。
  - 失敗した確認は `::error` の注釈にも出す（公開リポジトリの注釈はログイン無しで読める）。
- `docs/guide/testing.md`（英日）の「継続的インテグレーション」を workflow の表にした。

### 確かめたこと

- workflow の YAML: `yaml.safe_load` と actionlint 1.7.12（docker の `rhysd/actionlint`。shellcheck を含む）で警告 0。
- 同じ手順を実機で回した（GitHub Actions は rc まで回せないため。同期した 5f06fd65 で）:
  - macOS（ssh llm、arm64、Homebrew の python3・openssl・protoc）: `library_e2e.sh c` と `python` が ALL OK。
  - Windows（ssh aofish、x86_64、Git Bash から `PY3=python`、curl.se の curl 8.22・protoc を `PATH` の先頭）: `c` と `python` が
    ALL OK。`Start-Process -WindowStyle Hidden` で起こした bash の中の cargo はビルドスクリプトが `STATUS_DLL_INIT_FAILED`
    で落ちた（ウィンドウステーションの無い起動の問題。ssh から直接起こせば通る。CI の runner には関係しない）。

### 決めたこと

- `ci.yml` の job ではなく別の workflow にした（GitHub Actions は job 単位の `paths` を持たない。`paths` で `src/`・`crates/`・
  `examples/api/`・`tests/api/`・マニフェスト・同梱の Swagger UI に触れたときだけ回す）。
- v0.10.0 の WebSocket・multipart は、それぞれのチケット（F-261 等）が `library_e2e.sh` の `c`・`python`（`crates/veil-python/tests/e2e.sh`）
  に確認を足せば、この job で macOS・Windows でも回る。async は既に入っている。
- 重い試験（Autobahn・Playwright の Swagger UI のオフライン・FastAPI 互換の全件）は、それぞれのチケットで `examples-nightly.yml`
  に足す（F-264 の Swagger UI のオフラインの試験はそこで足す）。
- Windows arm64 の wheel（`windows-11-arm`）は足さない（rc の前に runner で試せないため。記録だけ）。
- 所要時間は rc の初回の実行で測る（手元・検証機は依存のキャッシュがあるので目安にならない）。長すぎれば `c` と `python` を
  別の job に分ける。

