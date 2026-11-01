# B-264: macOS のサンドボックスの下で File ルートの相対パスが解決できず、例の設定が全部 404 になる（v0.10.0-rc.1 で発覚）

**状態: 修正済み（v0.10.0）**

## 出典

`v0.10.0-rc.1` の `release.yml`（`dist / smoke macOS macos-14`・`macos-15-intel`）の `tools/release/binary-smoke.sh` で、
`-c examples/config.toml`・`VEIL_CONFIG` のどちらでも `GET /`・`GET /quickstart.html`・h2c が 404 になった（存在しないファイルの 404 は正しい）。
同じ rc で `veil-api-kit (wheel and sdist)` も落ちた（下の「同じ rc の別件」）。

## 原因

- 例の設定は `path = 'examples/www/'`（相対パス）。macOS は静的配信のパスを `canonicalize()` で解決する（`openat2`・capsicum が無い）。
- F-282 SB-12 で Seatbelt の読み取りを設定が指すパスとシステムのパスの `subpath` に絞った。その下では相対パスの `realpath` が
  `getcwd` の段で拒否される（作業ディレクトリ自体の読み取りを許しても通らない。`sandbox-exec` で確かめた）。絶対パスなら通る。
- PR の CI はサンドボックスを掛けた macOS の配布物でこの手順を回さない（`bindings` の macOS はライブラリの試験）。

## 修正（v0.10.0）

- macOS だけ、設定ロード（`config::loaded`）で File ルートの相対パスを最初のロード（サンドボックスの前）の作業ディレクトリからの
  絶対パスにする（`absolutize_file_root`）。リロードは同じ作業ディレクトリを使う。サンドボックスのプロファイル
  （`collect_macos_sandbox_paths`）も同じ作業ディレクトリで絶対パスにしているので、許可する範囲は変わらない。
- 試験: 単体 `config::loaded::macos_file_root_tests`（macOS）。macOS 実機で `binary-smoke.sh`（ALL OK）と SIGHUP のリロード後の 200 を確かめた。

## 同じ rc の別件

- `release.yml` の `veil-api-kit` のジョブが、`sorted(glob(...))` の先頭を sdist と決めつけていた（`-py3-none-any.whl` の方が
  `.tar.gz` より前に並ぶ）。順序に依らず 1 つずつあることを確かめるようにした。

## 関連

- F-282（SB-12）、[F-276](../features/F-276-v0100-verification-and-release.md)、[B-263](B-263-nightly-advisories-and-musl-test-build-v0100.md)。
