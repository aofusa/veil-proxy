# B-167: 配布する Windows の veil.exe が VCRUNTIME140.dll を読み込み、素の Windows で起動しない

## 出典

v0.9.1 の winget の最初の提出（microsoft/winget-pkgs#461268）。検証の pipeline が MSI を入れた後の起動の確認で
`Validation-Executable-Error` を付けた（2026-10-25）。

## 事象

- zip・MSI の `veil.exe`（x86_64・aarch64 とも）の import 表に `VCRUNTIME140.dll` と `api-ms-win-crt-*.dll` がある
  （C ランタイムを動的にリンクしていた。Rust の `*-pc-windows-msvc` の既定）。
- `VCRUNTIME140.dll` は Visual C++ 再頒布パッケージで入る DLL で、素の Windows には無い。無いと
  「VCRUNTIME140.dll が見つからない」で起動できない。winget の検証の sandbox がこれに当たった。

## 見逃した理由

- dist.yml の `windows-test`（windows-2022・windows-11-arm）と手元の検証機（aofish）には、Visual Studio などが
  再頒布パッケージを入れていた。binary-smoke・test-msi・winget の試験はどれも通っていた。

## 修正

- `docker/Dockerfile.windows`（配布物を作るクロスビルド。cargo-xwin）で `CARGO_TARGET_*_PC_WINDOWS_MSVC_RUSTFLAGS` に
  `-C target-feature=+crt-static`。cc・cmake のクレートは crt-static を見て C の依存（aws-lc-sys・boring-sys・
  libmimalloc-sys）も `/MT` で組み立てる。ただし cargo-xwin（0.23）が作る CMake の toolchain は
  `set(CMAKE_MSVC_RUNTIME_LIBRARY CACHE STRING "MultiThreaded")` で値が説明の欄に入り、変数が空のまま CMake の既定（`/MD`）で
  BoringSSL（boring-sys）が組み立てられる（lld-link の `/failifmismatch: RuntimeLibrary`、quiche の `tls_method.cc.obj` が
  `MD_DynamicRelease`）。cmake の crate は環境変数 `CMAKE` の実行ファイルを使うので、構成のときだけ
  `-DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded` を足す wrapper（`/usr/local/bin/cmake-msvc-static`）を置く。BoringSSL は
  `cmake_minimum_required(VERSION 3.10)` で CMP0091 が未設定（変数を見ずにフラグの既定の `/MD` を使う）なので、
  `-DCMAKE_POLICY_DEFAULT_CMP0091=NEW` も足す。
- `.cargo/config.toml` には置かない（同じ木から作る Python の wheel の拡張や、Windows の手元のビルドの CRT は変えない）。
- 試験: `tools/release/check-pe-imports.py`（PE の import 表に C ランタイムの DLL が無いこと。dist.yml の cross の Windows）と、
  `windows-test` の x86_64 で Nano Server のコンテナ（再頒布パッケージが無い）で `veil.exe --version`。

## winget

- 0.9.1 のマニフェストは依存（`Microsoft.VCRedist.2015+.x64`・`.arm64`）を足して出した。
- 次の版からは静的にリンクするので依存は要らない（komac は前の版のマニフェストを引き継ぐので、依存が残っても害は無い）。
