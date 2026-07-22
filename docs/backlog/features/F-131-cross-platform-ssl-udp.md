# F-131: クロスプラットフォーム TLS (aws-lc-rs)・Windows UDP モジュール・BoringSSL quiche 対応

## 目的

クロスプラットフォーム（macOS, Windows x86_64/aarch64, OpenBSD）における TLS 暗号プロバイダ・UDP ネットワーク層・HTTP/3 暗号バックエンドの制約を解消し、全 OS・全アーキテクチャで `full` features ビルドを完全対応させる。

## 概要・改修内容

1. **macOS / Windows x86_64 の TLS 暗号プロバイダ統一 (`aws-lc-rs`)**:
   - `Cargo.toml` および `src/tls_provider.rs` のプロバイダ選択ロジックを更新し、macOS (`universal2` / `x86_64` / `aarch64`) および Windows x86_64 で `ring` ではなく `aws-lc-rs` (`aws-lc-sys`) を使用するよう統一。
   - クロスビルド環境の要件整理とビルドオプション適用（Windows x86_64 向け `nasm` / `cmake` 経路の確立、macOS 向けリンカ・ビルド設定の調整）。

2. **`quiche` (HTTP/3) 暗号バックエンドの `boringssl` 切り替え**:
   - OpenBSD、Windows (x86_64/aarch64)、macOS について、`http3` 有効化時の `quiche` 暗号バックエンドを `aws-lc-sys` 共有から分離し、本家 `boringssl` (`boring`) を直接使用する構成へ更新。

3. **Windows UDP ソケット層の実装**:
   - `src/runtime/udp.rs` および WSAPoll reactor 内に Winsock 対応非同期 UDP ソケット抽象 (`WSASocketW`, `sendto`, `recvfrom`, `ioctlsocket(FIONBIO)`) を新設。
   - Windows における `http3` および `l4-proxy` (UDP) を完全解放。

4. **全プラットフォーム `full` features ビルド完遂と packaging / docs 更新**:
   - OpenBSD, Windows (x86_64/aarch64), macOS (x86_64/aarch64) の全ターゲットで `full` features ビルドを検証・完遂。
   - `packaging/scripts/build-cross.sh` および `README.ja.md` / `AGENTS.md` 等のドキュメントを更新。

## 受け入れ条件

- [ ] `docs/backlog/backlog.md` に F-131 チケットを追加。
- [ ] macOS および Windows x86_64 で `aws-lc-rs` を指定したデフォルト features ビルドが成功する。
- [ ] OpenBSD, Windows, macOS で `quiche` が `boringssl` バックエンドで正常にビルドできる。
- [ ] Windows 上で UDP 非同期ソケット層が動作し、`http3` / `l4-proxy` が有効化される。
- [ ] OpenBSD, Windows (x86_64/aarch64), macOS (x86_64/aarch64) の全ターゲットで `full` features リリースビルドが成功する。
- [ ] `packaging/scripts/build-cross.sh` および関連ドキュメントが更新され、適切な粒度で git コミットされている。
