# F-78: OSS-Fuzz 連携の検討

親: [F-72](F-72-security-testing-further-hardening.md) 項目 4 / [F-52](F-52-cargo-fuzz-libfuzzer.md)。優先度 P3（外部インフラ依存）。

## 目的

既存 fuzz ターゲット（hpack/frame/header/config/wasm_abi/wasm_host_abi）を OSS-Fuzz へ
載せ、継続ファジングとクラッシュ自動起票を外部インフラで得る。

## 実装済み（プロジェクト定義・2026-07-06）

- **`tools/oss-fuzz/`** に OSS-Fuzz プロジェクト定義一式を用意:
  - `project.yaml`（language: rust / sanitizer: address / engine: libfuzzer / x86_64）
  - `Dockerfile`（`base-builder-rust` + cmake/nasm、ソースを `$SRC/veil` へ）
  - `build.sh`（`cargo fuzz build -O` で全ターゲット → `$OUT`。WASM ターゲットは
    `--features wasm` で別途ビルド。**F-80 回帰 seed を `<target>_seed_corpus.zip` で添付**）
  - `README.md`（`infra/helper.py` でのローカル検証手順・申請フロー）
- 対象: `hpack_decode`/`config_toml`/`http2_frame_decode`/`http_header_validate`
  + `wasm_abi`/`wasm_host_abi`。

## 残件

- OSS-Fuzz リポジトリを clone しての `infra/helper.py build_fuzzers` 実走（要 docker + 外部リポ）。
- 上流 OSS-Fuzz への New Project PR 申請（外部インフラ・承認プロセス依存、P3）。

## 受け入れ条件

- [x] OSS-Fuzz 規約準拠のプロジェクト定義（project.yaml/Dockerfile/build.sh）を用意。
- ローカルビルド `infra/helper.py build_fuzzers` の実走確認（docker 環境で実施＝残件）。
