# F-136: FreeBSD / OpenBSD の TLS 証明書ホットリロード（サンドボックス下）

## 目的

TLS 証明書ホットリロード（F-03/F-105）を、**サンドボックス（FreeBSD capsicum capability
mode / OpenBSD pledge+unveil）有効下でも**動作させる。従来は capability mode に入ると
`std::fs::metadata`/`File::open` が絶対パス操作のため `ECAPMODE` で失敗し続け、
証明書リロードが恒常的に無効になっていた。設計の詳細・実験結果・却下案は
[docs/artifacts/f136_platform_design.md](../../artifacts/f136_platform_design.md) の
「F-136」節を参照（本チケットはその実装記録）。

## 現状の問題（改修前）

1. **FreeBSD capsicum**: `cap_enter(2)` 後は絶対パスの `open`/`stat` が `ECAPMODE` で禁止される
   ため、`TlsCertReloader::check_and_reload()`/`reload_now()`（H1/H2 rustls 用 `ServerConfig`
   再構築）が失敗し続ける。
2. **HTTP/3（quiche）**: `Config::load_cert_chain_from_pem_file(path)` はパス指定 API であり、
   内部で最終的に `fopen(3)`（=`open(2)`）を呼ぶ。memfd + `/proc/self/fd/N`（Linux）/
   fdescfs・一時ファイル（FreeBSD, B-50）という既存の迂回策も**いずれも最後にパス open を
   する**ため、capability mode では原理的に破綻する。

## 改修内容

### (A) H1/H2（rustls）: cert/key 読み取りの dirfd 相対化

- `src/security.rs` の `capsicum` モジュールに、F-123（capability mode 下の静的配信）と
  同じ仕組みで **cert/key それぞれの親ディレクトリ dirfd** を保持する
  `init_tls_cert_dirfds(cert: &Path, key: &Path)` を追加。`cap_enter` 前に呼ぶ。
  内部で `open_tls_pem_ro`（`openat` + `O_RESOLVE_BENEATH`）/ `stat_tls_pem`（`fstatat`）を
  提供し、登録済みパスに一致した場合のみ dirfd 相対化する。
- `src/entry.rs` の `cap_enter` 呼び出し前（`init_static_dirfds` の隣、capsicum 有効時の
  cap-enter 監視スレッド内）で `tls_auto_reload` が有効なときのみ呼ぶ。
- `src/tls_reload.rs` に単一チョークポイント `read_pem(path) -> io::Result<Vec<u8>>` /
  `pem_mtime(path) -> io::Result<SystemTime>` を追加。FreeBSD ビルドでは
  `security::capsicum::open_tls_pem_ro`/`stat_tls_pem` が登録済みパスにヒットした場合のみ
  dirfd 経由、それ以外（未登録パス・非 FreeBSD）は従来通り `std::fs`。
  `combined_mtime`/`reload_http3_certs`（HTTP/3 用 PEM 再読込）をこれらへ差し替え。
- `src/config.rs::load_tls_config`（H1/H2 `ServerConfig` 構築。起動時・リロード時の両方から
  呼ばれる単一関数）の `File::open` を `tls_reload::read_pem` + `BufReader::new(&[u8])` に
  差し替え。**Linux/macOS/Windows/OpenBSD はバイト等価（`std::fs::read` と同一結果）で
  挙動不変**。

### (B) HTTP/3（quiche）: in-memory SSL_CTX 化（FreeBSD / OpenBSD、副次的に macOS / Windows も対象）

- `quiche::Config::with_boring_ssl_ctx_builder`（`boringssl-boring-crate` feature 限定）を使い、
  `Http3CertMaterial` が保持する PEM バイト列から `boring::ssl::SslContextBuilder` を直接組んで
  `Config` を構築する。ファイル・パス・memfd を一切介さないため、capsicum capability mode /
  pledge+unveil のいずれの下でも動作する。
- `src/http3_server.rs`:
  - `new_quic_config_with_certs(cert_pem, key_pem) -> io::Result<Config>` を
    `#[cfg(target_os = "linux")]`（従来の memfd 経路、**1 行も変更しない**）と
    `#[cfg(not(target_os = "linux"))]`（in-memory SSL_CTX、新規）に分離。
  - QUIC トランスポートパラメータ設定を `configure_quic_transport()` として独立関数に切り出し
    （初回ロード・リロード共通）。
  - `reload_quiche_certs()`: Linux は従来通り既存 `Config` に対して
    `load_cert_chain_from_pem_file`/`load_priv_key_from_pem_file` を呼び直すだけ（無変更）。
    Linux 以外は `with_boring_ssl_ctx_builder` が新規 `Config` しか返せない制約上、
    新規 `Config` を構築し `configure_quic_transport` で初回ロードと同じパラメータを
    再適用したうえで `RefCell` の中身を丸ごと入れ替える（初回ロード・リロードで
    同じ in-memory 経路を通る）。

### Cargo.toml / .cargo/config.toml の変更

- `[target.'cfg(any(target_os = "linux", target_os = "freebsd"))'.dependencies]` を
  **`target_os = "linux"` のみ**に縮小（quiche は引き続き `default-features = false` +
  共有 `aws-lc-sys`、NO_PREFIX=1）。
- `[target.'cfg(not(any(target_os = "linux", target_os = "freebsd")))'.dependencies]` を
  **`not(target_os = "linux")`** に拡張し、quiche の feature を `["boring"]` →
  **`["boringssl-boring-crate"]`** に変更。`boring = "4.3"`（quiche 0.24.9 が依存するバージョン
  と一致）を同ブロックへ追加（`src/http3_server.rs` が直接 `boring::ssl::*` を使うため）。
- `.cargo/config.toml`: `AWS_LC_SYS_NO_PREFIX_x86_64_unknown_freebsd` /
  `..._aarch64_unknown_freebsd` を `"1"` → `"0"` に変更（aws-lc-sys と BoringSSL の
  シンボル衝突回避。理由コメントを B-47 と同じ体裁で追記）。

### なぜ aws-lc-sys 共有のままでは実現できなかったか

`aws-lc-sys`（`NO_PREFIX=1`）+ `boring` crate + `quiche/boringssl-boring-crate` を同一バイナリに
同居させる実験で、リンク段階に `AES_encrypt` 等の重複シンボルエラーが発生することを確認した
（AWS-LC は BoringSSL のフォークであり、NO_PREFIX=1 で FIPS モジュール由来のシンボルを素の名前で
export するため）。詳細は設計メモ参照。

## テスト

- 単体テスト: `src/tls_reload.rs` の既存テスト（mtime 検知・HTTP/3 世代配信）は変更なしで通過
  （`read_pem`/`pem_mtime` は非 FreeBSD で `std::fs` と完全等価のため回帰なし）。
- `cargo build --features full`（Linux）: 無変更で成功。
- `cargo tree --features full | grep -i boring`: **空**（Linux に boring が入っていないことを
  確認）。
- `cargo test --lib --features full`: 既存テスト回帰なし。
- `cargo clippy --features full --all-targets -- -D warnings`: 警告なし。
- `cargo build --no-default-features`: 成功。
- `cargo check --target x86_64-unknown-freebsd --no-default-features --features full-freebsd`:
  型チェックが通ることを確認（実バイナリ生成は QEMU 側で別途検証）。
- 実機/VM 検証（capsicum capability mode 有効 + HTTP/3 での証明書リロード動作確認）は
  `tools/qemu/bsd-vm.sh` を用いて別途実施する（本チケットのローカル作業範囲外）。

## 受け入れ条件

- [x] `docs/backlog/backlog.md` に F-136 チケットを追加。
- [x] Linux の `cargo build --features full` / `cargo test --lib --features full` /
      `cargo clippy --features full --all-targets -- -D warnings` が無変更で通る。
- [x] `cargo tree --features full | grep -i boring` が空（Linux に boring 依存が入らない）。
- [x] `cargo build --no-default-features` が通る。
- [x] README.md / docs/readme/README.ja.md / AGENTS.md の quiche バックエンド target 別記述を
      更新し、実際の Cargo.toml と整合させる。
- [ ] QEMU 上の FreeBSD/OpenBSD で capsicum/pledge 有効 + HTTP/3 の証明書リロード実地検証
      （別途実施、本チケットの完了はローカル検証まで）。

## QEMU 実地検証（2026-10-10 追記）

F-176 の BSD security-e2e で確認済み。FreeBSD 14.3 aarch64 は capsicum capability mode 下で `[tls] auto_reload` による
証明書リロードが成功し、OpenBSD 7.9（aarch64 / x86_64）は pledge + unveil 下で SIGHUP による証明書リロードが成功した。
capability mode 下の SIGHUP による **設定** リロードは対象外で、F-178 で扱う。
