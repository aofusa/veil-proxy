# B-400: nightly の container_security が rustix の勧告・Wolfi の glibc の CVE で落ち、その奥の quinn-udp の勧告も照合で見逃していた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

main の `container-security-nightly` が 2026-11-12 から毎晩落ちていた（run 43125476110・43327661886・43526383503）。

## 事象と原因

1. `run.sh (glibc)`・`run.sh (musl)` の `cargo_audit` が、本体の `Cargo.lock` の rustix 1.1.4 の RUSTSEC-2026-0486
   （unsound。xattr の関数が長さ 0 のバッファで書いていないバイトを初期化済みにする。直した版は 1.1.6）で落ちた。
   例・試験用のクレート・VDS の参照サーバの 11 個の `Cargo.lock` も rustix 1.1.4・1.1.5 だった。
2. `SBOM + vulnerability scan` の grype が、glibc のイメージの CVE-2026-84243（High）で落ちた（2026-11-12 から）。
   B-330 の除外はパッケージ `glibc-2.44` に限っていたが、DB の更新で同じ CVE が `glibc-2.44-locale-posix`・`ld-linux-2.44`
   にも付いた。Wolfi は 2.44-r16 で直している（B-330 の「直った版が出たら除外を消す」の時期）。
3. `run_cargo_audit.sh` は `set -euo pipefail` で最初の `Cargo.lock` の失敗で止まるので、その奥は照合されていなかった。
   全部を見ると、`tools/perf/h3load/Cargo.lock` の quinn-udp 0.5.15 と vendoring 版（`third_party/quinn-udp`、0.5.14）が
   RUSTSEC-2026-0538（1 回の `recvmsg` で 1 データグラムを読む target＝OpenBSD・NetBSD・macOS 等で、切り詰められたデータグラムの
   後の `EAGAIN` でも古い `MSG_TRUNC` を見て受信のループが回り続ける。直した版は 0.5.16）に該当した。
4. vendoring 版の GitHub の勧告の照合（`vendored_ghsa_audit.py`、F-277）は、quinn-rs/quinn のリポジトリの勧告
   GHSA-6pp4-4cxf-xf88（3 の別名）を見逃していた。上流は範囲を「`< 0.5.16, >= 0.6.0 < 0.6.3`」と系列ごとに並べて書いており、
   カンマを「全部を満たす」と読むと誰も該当しない（偽の OK）。「`0.11.0 - 0.11.6`」の形も 0.11.0 との一致だけと読んでいた。

## 修正

1. 12 個の `Cargo.lock` の rustix を 1.1.6 にした（版と checksum だけ。`cargo update -p rustix` は windows-sys の依存の辺まで
   0.61.2 から 0.52.0・0.60.2 へ組み替えるので使わず、各 `Cargo.lock` を `cargo metadata --locked` が変えずに受け付けることを確かめた）。
2. `cgr.dev/chainguard/glibc-dynamic` のダイジェストを glibc 2.44-r19 のもの（`a5e99919…`）にした（`Dockerfile.glibc`・
   `Dockerfile.glibc.aarch64`・`Dockerfile.vds-server`）。`.grype.yaml` の除外を消した（`ignore: []`）。
3. vendoring 版の quinn-udp を 0.5.16 にした（0.5.14 → 0.5.16 の上流の差分を `patch` で当てた。veil の差分は変わらない。
   `README.veil.md` に上げ方を書いた）。`tools/perf/h3load/Cargo.lock` の quinn-udp を 0.5.16 にした。
4. `vendored_ghsa_audit.py` の範囲の解釈: 並べた制約を全部満たす版が無いときだけ、上限（か一致）の後の下限・一致を別の範囲の
   始まりとして読む（`<= 6.0.0, >= 1.0.0` のように上限を先に書く範囲は従来どおり全部を満たす）。`A - B` は両端を含む範囲。
   読めない範囲・矛盾する範囲は「該当しない」と見なさず該当として報告する。`test_vendored_ghsa_audit.py` を足し、`ci.yml` の
   messages の job で回す。

## 確認

- 12 個の `Cargo.lock`（と quinn-udp の 2 個）を `cargo metadata --locked` が変えずに受け付ける。
- cargo-audit（`--deny warnings`）を `run_cargo_audit.sh` と同じ手順で、追跡している全部の `Cargo.lock` と vendoring 版の仮の
  `Cargo.lock` に掛けて 0 件（修正前は rustix・quinn-udp の 2 件）。
- grype 0.97.1（CI と同じ版）で、旧ダイジェストの amd64 の層は CVE-2026-84243 と GHSA-8rvm-662m-jvqv が 3 パッケージずつ（CI と同じ）、
  新ダイジェストの amd64・arm64 は 0 件。`.grype.yaml`（`ignore: []`）を読ませても同じ。
- `vendored_ghsa_audit.py`: 修正前の README（0.5.14）で GHSA-6pp4-4cxf-xf88 を MATCH にし、現在の木は 0 件（quiche の既知の
  ignore 3 件だけ）。wasmtime の上限を先に書いた範囲（GHSA-ff4p-7xrq-q5r8 等）は 36.0.20 に該当しない。
- vendoring 版の quinn-udp の上流の単体テスト（`recv_single_after_truncation_returns_would_block_instead_of_spin` 等 4 件）が通る。
  修正前の受信のループに戻すと、このテストが失敗する。macOS（llm）では修正を確かめる試験は毎回通り、
  `recv_single_skips_truncated_datagrams` だけがループバックの非同期の配送で 20 回中 9 回 `WouldBlock` になる（受ける前に 20ms
  待たせると 20 回とも通る。試験の競合でコードの誤りではない。`README.veil.md` に書いた）。
- macOS で bindings の job と同じ `library_e2e.sh c c-kit python` が通る。
