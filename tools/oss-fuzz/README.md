# OSS-Fuzz 連携（F-78）

Veil の cargo-fuzz ターゲットを [OSS-Fuzz](https://github.com/google/oss-fuzz) の
継続ファジングに載せるためのプロジェクト定義。

## ファイル

| ファイル | 役割 |
|----------|------|
| `project.yaml` | プロジェクトメタ（language: rust、sanitizer: address、engine: libfuzzer） |
| `Dockerfile` | `base-builder-rust` + cmake/nasm。ソースを `$SRC/veil` へ配置 |
| `build.sh` | `cargo fuzz build` で全ターゲットをビルドし `$OUT` へ配置。F-80 回帰 seed を seed_corpus.zip で添付 |

## 対象ターゲット

`hpack_decode` / `config_toml` / `http2_frame_decode` / `http_header_validate`
（既定 features）と `wasm_abi` / `wasm_host_abi`（`--features wasm`）。

## ローカル検証手順

OSS-Fuzz リポジトリを clone し、本ディレクトリを `projects/veil/` として配置する:

```bash
git clone https://github.com/google/oss-fuzz
cp -r tools/oss-fuzz oss-fuzz/projects/veil
cd oss-fuzz
python infra/helper.py build_image veil
python infra/helper.py build_fuzzers --sanitizer address veil
python infra/helper.py check_build veil
python infra/helper.py run_fuzzer veil hpack_decode
```

## 申請

`check_build` が通ることを確認後、OSS-Fuzz へ New Project PR を出す
（`primary_contact` の登録メールが CC を受ける）。クラッシュは OSS-Fuzz が
自動 issue 化し、回帰 seed は [F-80](../../docs/backlog/features/F-80-regression-corpus.md)
の `fuzz/regression_corpus/` へ還流する。
