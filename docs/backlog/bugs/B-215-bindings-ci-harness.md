# B-215: bindings の CI（Windows・macOS）で library_e2e が落ちる（色付きの native-static-libs・HTTP/2 の無い curl・listen の待ち行列）

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の PR の CI（`bindings.yml` の `bindings windows-x86_64`・`bindings macos-arm64`。2026-10-27）。手元の Windows・macOS では通っていた。

## 事象と原因

- C の例のリンクが `LNK1104: cannot open file 'msvcrt\e[0m.lib'`（Windows）・`ld: library 'm\e[0m' not found`（macOS）で落ちた。
  CI は `CARGO_TERM_COLOR=always` なので、`cargo rustc -- --print native-static-libs` の行の末尾に色の終わりのエスケープが付き、
  ハーネスがそれごとライブラリ名として拾っていた。
- Windows の h2c の確認（C・Python の 21 件）が `the installed libcurl version does not support this` で落ちた。choco の curl が
  8.23 から nghttp2 無しのビルドになった。
- macOS の asyncio 2,000 並行・接続 2,000 本が `ConnectionResetError` で落ちた。macOS の `kern.ipc.somaxconn` の既定 128 で
  listen の待ち行列が溢れる（NetBSD と同じ）。ハーネスは root のときだけ上げていた。

## 修正

- `cargo rustc --color never` で聞く。
- Windows は curl.se の公式ビルド（版を固定）を入れ、`curl -V` に HTTP2 が無ければ試験の前に止める。
- listen の待ち行列は root でなくてもパスワード無しの `sudo -n` が使えれば上げる（macOS は `kern.ipc.somaxconn`）。
