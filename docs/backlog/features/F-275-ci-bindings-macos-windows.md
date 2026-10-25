# F-275: Python の wheel と C のライブラリを macOS・Windows でも CI で作って試す

**状態: 未着手（2026-10-25 起票・設計。親は [F-258](F-258-v0100-plan.md)）**

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
