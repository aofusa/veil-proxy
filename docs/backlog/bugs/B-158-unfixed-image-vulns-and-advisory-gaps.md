# B-158: 直す版の無いイメージの脆弱性を見逃す設定と、本体以外の Cargo.lock の勧告

**状態: 完了（release/v0.8.4、2026-10-22。v0.8.4 で公開）**

## 現象

1. 公開イメージ `glibc-v0.8.3`（distroless/base-nossl-debian13）の `libc6` 2.41-12+deb13u4 に High が 2 件
   （CVE-2026-5435・CVE-2026-19499）と Medium 以下が多数あった。どれも Debian が「won't fix」としていて直す版が無い。
   B-157 で毎晩の grype を `only-fixed: true` にし、container_security の Trivy も `--ignore-unfixed` だったので、
   どの試験も失敗しなかった。`musl-v0.8.3`（scratch）は 0 件。
2. cargo-audit・cargo-deny はルートの `Cargo.lock` しか見ていなかった。ほかの Cargo.lock には、修正前の版が残っていた。

   | Cargo.lock | 勧告 |
   |---|---|
   | `examples/api/*`（4 つ）・`tools/api-bench/axum-veil`・`tools/tokio-compat/ecosystem` | rustls-webpki 0.103.15（RUSTSEC-2026-0382）・webpki-roots 1.0.9（RUSTSEC-2026-0381） |
   | `tests/api/grpc_client`・`tools/perf/h3load` | rustls-webpki 0.103.15・webpki-root-certs 1.0.9（RUSTSEC-2026-0383） |
   | `tests/test_backends` | rustls-webpki 0.103.15 |
   | `tests/grpc_server` | h2 0.4.13（RUSTSEC-2026-0258） |
   | `tools/vds/server` | rustls-webpki 0.103.15 |
   | `examples/wasm-filters/jwt-filter`（git 管理外の lock） | rsa 0.9（RUSTSEC-2023-0071。Marvin 攻撃。**修正版が無い**） |

   B-157 で「例の Cargo.lock も上げた」としていたのは誤りで、例の 4 つも 0.103.15 のままだった。
3. 警告扱いの勧告が残っていた。
   - 保守終了（unmaintained）: `fxhash`（RUSTSEC-2025-0057、wasmtime の `profiling` → `fxprof-processed-profile` 経由）、
     `rustls-pemfile`（RUSTSEC-2025-0134、VDS の参照サーバの tonic 0.12 経由）
   - unsound: anyhow 1.0.102（RUSTSEC-2026-0190）・rand 0.8.5（RUSTSEC-2026-0097）
   - yanked: num-modular 0.6.6・chacha20 0.10.1・spin 0.9.8

   cargo-deny は `unmaintained = "workspace"` で間接依存を見ず、cargo-audit は警告では失敗しなかった。

## 修正

**イメージ**

- glibc 版の土台を `cgr.dev/chainguard/glibc-dynamic:latest`（Wolfi）にした（`Dockerfile.glibc`・`Dockerfile.glibc.aarch64`）。
  - Wolfi の glibc は上流の修正を取り込んで更新される。grype で amd64・arm64 とも 0 件。
  - 中身は distroless と同じ最小構成（glibc・libgcc・CA 証明書・`/etc/passwd` の nonroot=65532、シェル無し）。
  - 既定のユーザーは nonroot だが、distroless と同じく root で起動して設定の `drop_privileges` で降格するよう
    `USER 0:0`・`WORKDIR /` を明示した。
- VDS の参照サーバのイメージ（`Dockerfile.vds-server`、distroless/cc-debian12）も同じ土台にした。

**脆弱性の検査（直す版の無いものも失敗にする）**

- 毎晩の grype（イメージの SBOM）: `only-fixed: false`。
- container_security の Trivy: `--ignore-unfixed` を外した。
- 公開の workflow（`release.yml`）に 2 つの関門を足した。
  - `advisories` ジョブ: cargo-audit・cargo-deny。イメージ・crates.io・PyPI のどの公開もこれが通らないと始まらない。
  - `image-test` ジョブ: 公開前の各イメージ（4 つ）を grype で調べる。High 以上で失敗（`only-fixed: false`）。
- cargo-audit（`run_cargo_audit.sh`）: `--deny warnings` にして、リポジトリ内のすべての Cargo.lock を見る
  （`target/`・`third_party/` を除く）。
- cargo-deny: `unmaintained = "all"`・`yanked = "deny"`。

**依存**

- wasmtime の `profiling` を外した。veil は使わない。
  - 本体: crates.io の `wasmtime`・vendoring 版とも `default-features = false` にし、`profiling` 以外の既定を並べた。
  - vendoring 版（`third_party/wasmtime`）: `profiling`・`profile-pulley` の feature と、`fxprof-processed-profile`・
    `ittapi` の依存を消した。ワークスペースの暗黙のメンバーなので、使わない任意の依存も `Cargo.lock` に載るため。
    経緯は `README.veil.md`。
- 本体以外の Cargo.lock: 上の表の依存と anyhow・rand 0.8・num-modular・chacha20 を `cargo update -p` で修正版へ。
- VDS の参照サーバ: tonic 0.12 → 0.14（`tonic-prost`・`tonic-prost-build`）・prost 0.14・axum 0.8。
  rustls-pemfile が依存から消えた。
- JWT の例のフィルタ（`examples/wasm-filters/jwt-filter`）: `rsa` クレートをやめ、RS256 の検証を `num-bigint` で書き直した。
  - 検証は公開鍵の演算（s^e mod n）だけなので、Marvin 攻撃（秘密鍵の演算の時間差）の対象ではない。それでも
    修正版の無い勧告の依存は残さない。
  - 復号した値は解析しない。期待する EMSA-PKCS1-v1_5 の符号化を組み立てて、バイト列ごと比べる（RFC 8017 §8.2.2）。
  - 鍵は 2048〜4096 ビット（RFC 7518 §3.3 の下限）、e は奇数で 33 ビット以下。
  - openssl で作った試験のベクタを足した（正しい署名・本文や署名の改ざん・長さ違い・弱い鍵）。

## 確認

- `run_cargo_audit.sh`（全 Cargo.lock、警告も失敗）: 0 件。
- `run_cargo_deny.sh`: `advisories ok, licenses ok`。
- grype: 新しい glibc の土台（amd64・arm64）・musl イメージ（scratch）とも 0 件。
- 新しい glibc イメージ（`full-container`）:
  - `tools/release/image-smoke.sh`（`container`・`defaults`）。
  - container_security: Trivy（直す版の無いものも数える）で HIGH・CRITICAL 0 件。全フェーズ。
    この実行で、メトリクスの系列が際限なく増える問題（B-159）と、集計が失敗を passed に数える問題が見つかった。
- VDS の standalone E2E（Rust の参照サーバ）。
- jwt-filter の単体試験（25 件 + 結合 8 件）。`RUSTFLAGS=-Clink-arg=--allow-undefined` で wasm32-wasip1 のビルド。

## 残っていること

- `examples/wasm-filters/*` は、rustc 1.99 の rust-lld では `proxy_*` の未定義シンボルのためにリンクできない。
  - 変更の前から、すべてのフィルタで起きている（`--allow-undefined` を付ければ通る）。
  - 脆弱性とは別の問題として扱う。
- h3spec の 49 件中 11 件の不一致は B-100 の時点（38/49）から変わっていない。調べた結果、安全性の問題ではなかった。
  - トランスポートパラメータの 8 件: quiche は不正な値を検出している（veil のログに `recv error: InvalidTransportParam`）。
    - その接続で 1 つもパケットを処理できていないうちのエラーなので、quiche は CONNECTION_CLOSE を送らずにその場で閉じる
      （`Connection::close` の `recv_count == 0`）。でたらめな最初のパケットに応答しない設計。
    - tcpdump でも、サーバは何も返さず、クライアントの再送はそれぞれ新しい接続として同じく捨てられた。
    - h3spec が求めるのは TRANSPORT_PARAMETER_ERROR を送ることなので、エラーの伝え方の準拠の差。
  - 0-RTT の CRYPTO（1 件）: 接続は閉じる。エラーの種類が違うだけ（TLS の unexpected_message）。
  - QPACK の 2 件: quiche は動的テーブルを使わないので、エンコーダ・デコーダのストリームの値は状態に効かない。
    こちらもエラーの伝え方の差。
