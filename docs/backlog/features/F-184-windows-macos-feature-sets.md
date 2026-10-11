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

## 改修前後の性能比較（F-183 + F-184）

基準は F-183 マージ前の main（30ff117、mimalloc・FreeBSD は既定の libc jemalloc）、比較対象は本ブランチ
（Linux = veil-alloc、FreeBSD = チューニング済み libc jemalloc）。どちらも交互 A/B。

### Linux（Docker、`tools/perf/run_perf.sh`、3 ラウンド・ビルド順を入れ替え）

基準は `git worktree` の 30ff117 から `docker/Dockerfile.glibc` でビルドした `veil:f183base`
（B-113 のテスト修正だけ当てた）、現行は `veil:f184`。代表 15 構成（静的配信 h1/h2・プロキシ・圧縮・
キャッシュ・Wasm・HTTP/3・gRPC・WebSocket・L4・h2c）を `CONFIGS` で指定し、中央値で比べた。

| 構成 | proto | 基準 rps | 現行 rps | 差 | メモリ（MB、基準→現行） |
|---|---|---|---|---|---|
| 静的 ofc=1 | http1.1 / http2 | 9,944 / 7,388 | 9,953 / 7,463 | +0.1% / +1.0% | 28.2→19.4 / 76.4→59.4 |
| 静的 ofc=0 | http1.1 / http2 | 8,483 / 6,733 | 8,432 / 6,989 | −0.6% / +3.8% | 25.5→17.5 / 86.9→61.2 |
| プロキシ | http1.1 / http2 | 6,329 / 6,071 | 6,443 / 6,100 | +1.8% / +0.5% | 41.2→35.8 / 81.8→96.4 |
| 圧縮 | http1.1 / http2 | 8,329 / 4,072 | 8,390 / 3,810 | +0.7% / −6.4% | 25.3→17.5 / 85.9→86.4 |
| キャッシュ | http1.1 / http2 | 8,391 / 6,795 | 8,437 / 7,035 | +0.5% / +3.5% | 25.8→17.3 / 73.5→80.2 |
| Wasm | http1.1 / http2 | 8,444 / 6,854 | 8,424 / 6,912 | −0.2% / +0.8% | 40.5→38.7 / 88.3→94.5 |
| HTTP/3 | http3 | 1,872 | 1,858 | −0.8% | 135.3→115.9 |
| h3 プロキシ | http3 | 1,706 | 1,707 | +0.1% | 172.7→138.4 |
| gRPC | grpc | 5,132 | 5,162 | +0.6% | 28.2→20.9 |
| WebSocket | websocket | 3,230 | 3,216 | −0.4% | 20.8→13.7 |
| L4 | http1.1 | 13,025 | 13,072 | +0.4% | 28.8→20.6 |
| h2c 静的 / プロキシ | h2c | 23,464 / 10,199 | 24,365 / 9,933 | +3.8% / −2.6% | 59.3→32.0 / 89.2→60.1 |

失敗要求（non-2xx）は全構成 0。h2c プロキシはラウンドごとに上下が入れ替わる（+4.2% / −4.3% / −0.4%）ノイズ。
HTTP/2 圧縮の −6.4% は 3 ラウンド中 2 回で現行が低かったため、ローカルで切り分けた（次節）。

### Linux 圧縮経路の切り分け（ローカル、5 ラウンド、要求あたり CPU 時間）

基準（30ff117）・現行の mimalloc 版（`full` の veil-alloc を mimalloc に替えたもの）・現行の veil-alloc 版を
ホストでネイティブ起動し（2 コアに固定）、zstd / br / gzip × HTTP/2 / HTTP/1.1 を交互に測った
（`tools/perf/results/f184comp/comp_ab.sh`）。

| 負荷 | 基準 | 現行 mimalloc | 現行 veil-alloc |
|---|---|---|---|
| h2 zstd（rps / CPU/req） | 3,095 / 646us | +0.3% / −0.2% | **+1.3% / −1.1%** |
| h2 br | 561 / 3,687us | −3.0% / +1.5% | **+38.4% / −28.8%** |
| h2 gzip | 1,127 / 1,803us | −0.2% / 0.0% | +0.2% / −0.4% |
| h1 zstd / br / gzip（rps） | 8,998 / 8,867 / 8,866 | +0.5% / +0.7% / +0.6% | **+1.5% / +3.2% / +3.3%** |
| アイドル RSS / 負荷後 RSS | 24.7MB / 55.4MB | −27.8% / −18.6% | −28.9% / −16.6% |

veil-alloc は全負荷で基準・mimalloc と同等以上（brotli の大きな作業領域の確保・解放で特に速い）。
Docker で見えた HTTP/2 圧縮の差は再現せず、計測のノイズと判断した。

### FreeBSD 14.3 aarch64（ssh llm の VM、HVF）

`tools/perf/bsd/run_perf_bsd.sh`（54KB と 100B、3 ラウンド交互）は、VM のスケジューリングの揺らぎで
**nginx 自身の rps が回ごとに最大 2.7 倍ぶれる**ため、数 % の差の判定には使えない（中央値の差は
−19.6%〜+91.4% に散り、nginx 比で正規化すると符号が逆転する項目もあった）。F-183 と同じく
`tools/perf/bsd/alloc_ab_bsd.sh`（5 ラウンド交互）の **要求あたり CPU 時間** で比べた。

| 負荷 | rps（基準→現行） | CPU/req | 負荷中ピーク RSS |
|---|---|---|---|
| h1 TLS 54KB | 22,264 → 24,688（+10.9%） | 74.2 → 66.1us（−10.9%） | −20.2% |
| h1 TLS 3B | 120,432 → 121,547（+0.9%） | 14.5 → 14.3us（−1.3%） | −22.8% |
| h2 TLS 3B | 133,704 → 132,487（−0.9%） | 16.3 → 16.4us（+0.7%、±1 の範囲） | −20.8% |
| h1 プロキシ 3B | 72,904 → 74,820（+2.6%） | 23.1 → 21.9us（−5.0%） | −25.2% |
| h2c プロキシ 54KB | 20,543 → 20,209（−1.6%） | 102 → 104us（+1.5%、±6 の範囲） | −23.9% |
| h1 TLS 接続チャーン | 6,074 → 6,083（+0.2%） | 210 → 210us（0.0%） | −29.0% |

アイドル RSS 36.3 → 29.6MB（−18.5%）、負荷後 RSS 97.6 → 42.4MB（−56.6%）。失敗要求 0。

### 判断

Linux・FreeBSD とも退行は無い（rps・要求あたり CPU は同等以上、メモリは大きく減少）。Linux の `full` は
veil-alloc のままとし、mimalloc へは戻さない。
