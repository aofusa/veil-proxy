# F-184: Windows・macOS 向け feature セット（full-windows / full-macos）、アロケータ feature の排他化、macOS クロスビルドの修正

**状態: 完了（feat/f184-native-system-allocators）**

## 背景

- `full` は unix で veil-alloc（F-183）を使い、Windows では `full` に同梱した mimalloc へフォールバックしていた。
  Windows・macOS の配布ビルドのアロケータを、BSD と同じく専用の feature セットで明示したい。
- `full` が `veil-alloc` と `mimalloc` を両方含んでおり、アロケータ feature を 1 つだけ選ぶという規則から外れていた。
- `packaging/scripts/build-cross.sh --target macos` が依存の事前ビルド段で失敗していた（以前は `full` で
  ビルドできていた）。

## 改修内容

### 1. feature セット `full-windows` / `full-macos`

機能は `full` と同一で、アロケータだけを差し替えたセット。cargo にはターゲット別の default features が
無いので、BSD 向けの `full-freebsd` 等と同じく別 feature として用意し、配布ビルドが
`--no-default-features` と併せて明示する。

| セット | アロケータ |
|---|---|
| `full-windows` | mimalloc（veil-alloc は unix 専用） |
| `full-macos` | システムアロケータ（libmalloc。`system-allocator`） |

- `packaging/scripts/build-cross.sh`: `DEFAULT_MACOS_FEATURES="full-macos"`、`DEFAULT_WINDOWS_FEATURES="full-windows"`
- `docker/Dockerfile.macos` / `docker/Dockerfile.windows`: `ARG CARGO_FEATURES` の既定を同じく変更
- `default`（`ktls`・`http2`・`mimalloc`）は不変（AGENTS.md の規則）

経緯: 最初は build.rs で Windows・macOS の `mimalloc` を無効化し、`mimalloc-force` で戻す方式で実装したが、
指示により feature セット方式に切り替えた（81138a4 を 2492752 で revert）。Windows も当初はシステム
アロケータにしたが、指示により mimalloc にした。

### 2. アロケータ feature の排他化

`veil-alloc`・`mimalloc`・`jemalloc`・`system-allocator` は 1 つだけ選ぶ。`full` / `full-container` は
`veil-alloc` だけを含む（以前は `"veil-alloc", "mimalloc"`）。`cargo build --features full` では default の
mimalloc も有効になるが、優先順（veil-alloc > mimalloc > jemalloc）で veil-alloc が選ばれ、挙動は変わらない。

### 3. macOS クロスビルドの修正（vendoring 版 quiche の crate-type）

失敗の内容（`cacher` 段、`cargo zigbuild --target universal2-apple-darwin`）:

```
error: linking with `.../zigcc-aarch64-apple-darwin-....sh` failed: exit status: 1
  = note: error: unable to read exported symbols list '-dead_strip': FileNotFound
error: could not compile `quiche` (lib) due to 1 previous error
```

- quiche の `[lib] crate-type` は `["lib", "staticlib", "cdylib"]` で、cargo は依存クレートでも全種類をビルドする。
- ベースイメージ `messense/cargo-zigbuild`（タグ固定なし）が cargo-zigbuild 0.23.4 / zig 0.16.0 に上がり、
  macOS 向けの cdylib リンクで `-exported_symbols_list` の引数を取り違えるようになった。
- veil は quiche の Rust API しか使わないので、`third_party/quiche/Cargo.toml` を `crate-type = ["lib"]` にした
  （`third_party/quiche/README.veil.md` の差分 2）。リンク自体が無くなり、全ターゲットでビルドも短くなる。

### 4. クロスビルドの依存事前ビルドを dist プロファイルに揃える

全 Dockerfile（`docker/Dockerfile.{macos,windows,glibc,glibc.aarch64,musl,musl.aarch64}`）の cacher 段は
`--release`、builder 段は `--profile dist` でビルドしていた。
出力先（`target/<triple>/release` と `target/<triple>/dist`）が別なので事前ビルドは builder 段で一切使われず、
依存を 2 回コンパイルしていた（macOS で 37 分 × 2、ディスクも 2 倍で、検証中にホストのディスクを使い切った）。
全 Dockerfile の cacher 段を `--profile dist` にした（テストの `cargo test` は test プロファイルなので無関係）。

profile の指定方法は変えていない。F-147（ff7051b）で dist プロファイルを導入したときから、Dockerfile の
builder 段が `--profile dist` を直接書いており、packaging のスクリプトが profile を Dockerfile へ渡す仕組みは
過去にも無かった（コミット履歴で確認。profile を環境変数 `CARGO_PROFILE` で切り替えるのは
`tools/qemu/bsd-vm.sh` の BSD ネイティブビルドだけで、既定は release）。

## 検証

### Docker ビルド（`packaging/scripts/build-cross.sh`）

| ターゲット | feature | 結果 | 所要（cacher / builder） | 確認 |
|---|---|---|---|---|
| universal2-apple-darwin | `full-macos` | 成功（42.1MB、x86_64 + arm64 の fat binary） | 17 分 42 秒 / 8 分 2 秒 | builder 段のコンパイルは veil 本体だけ。バイナリに `mimalloc`・`veil_alloc` の文字列なし |
| x86_64-pc-windows-msvc | `full-windows` | 成功（26.4MB） | 9 分 47 秒 / 5 分 19 秒 | mimalloc のメッセージ文字列あり（mimalloc がリンクされている） |
| aarch64-pc-windows-msvc | `full-windows` | 成功（20.2MB） | 9 分 34 秒 / 5 分 31 秒 | 同上 |

修正前の macOS は cacher 段で `full` のまま失敗し、修正後も cacher（`--release`）37 分 39 秒のあと builder 段で
依存を全部ビルドし直していた。

### 実機での動作確認

- **macOS 15.5（Apple Silicon、ssh llm）**: universal2 バイナリで TLS の静的配信。HTTP/1.1 と HTTP/2 で 200、
  2000 接続の後も 200。RSS 23.9MB。`vmmap` ではシステムの libmalloc のゾーン（MALLOC_NANO / SMALL / MEDIUM）が
  使われている。macOS の LibreSSL が作る自己署名証明書は v1 で rustls が拒否するため、証明書は Linux 側で作った。
- **Windows 11（x86_64、ssh aofish）**: x86_64 バイナリで同じ構成。HTTP/1.1 は 8 並列 × 250 の 2000 要求すべて 200、
  HTTP/2 は `Invoke-WebRequest -HttpVersion 2.0` の 300 要求すべて 200（応答バージョン 2.0）。動作中の
  ワーキングセット 35MB。aarch64 の Windows 実機は無いので、クロスビルドの確認まで。

### Linux（cacher 段の変更の確認、`packaging/scripts/build.sh --docker`）

- glibc（`Dockerfile.glibc`）・musl（`Dockerfile.musl`）とも成功。builder 段のコンパイルは veil 本体だけで、
  コンテナ内の `cargo test --lib` は 1025 件すべて成功、tar.gz・deb・rpm まで作成できた。
- 最初の実行では `connect_broker::tests::connects_to_allowed_targets` が失敗した（B-113。F-182 のテストの前提の誤りで、
  コンテナ内では localhost の先頭が `::1` になる）。修正後に再実行して上記の結果。
- aarch64 の Dockerfile（`Dockerfile.glibc.aarch64` / `Dockerfile.musl.aarch64`）は同じ 1 行の変更で、
  ビルドは回していない（キャッシュ無しの Linux aarch64 は 10 時間を超えるため）。

### テスト

- `cargo test --lib --bins --test integration_tests --features full`: 単体 1025・統合 54 すべて成功。
- E2E（`./tests/e2e_setup.sh test`、io_uring）: 565 すべて成功。
- `cargo clippy --all-targets --features full -- -D warnings`: 警告なし。
- `cargo check --lib --no-default-features --features full-windows|full-macos|full` と `full,alloc-stats`: 成功。
