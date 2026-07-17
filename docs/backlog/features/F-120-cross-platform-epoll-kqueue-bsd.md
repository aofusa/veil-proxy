# F-120: クロスプラットフォーム対応（epoll フォールバック / aarch64 / FreeBSD / OpenBSD）

- 優先度: P1
- ステータス: 対応中
- 設計: [docs/artifacts/f120_cross_platform_design.md](../../artifacts/f120_cross_platform_design.md)

## 機能説明

io_uring 専用だったデータプレーンをコンパイル時バックエンド分離し、以下へ対応する。

1. **Linux epoll フォールバック**（`--features epoll`）: io_uring が使えない環境向け。
   デフォルトは従来どおり io_uring（`default` feature 不変・性能非劣化）。
2. **aarch64-unknown-linux**: docker/Dockerfile.{glibc,musl} ベースのクロスビルド +
   QEMU user-mode での E2E 動作確認。
3. **FreeBSD（x86_64/aarch64）**: kqueue ランタイム + capsicum（cap_rights_limit /
   cap_enter）+ jail 連携（packaging）。
4. **OpenBSD（x86_64/aarch64）**: kqueue ランタイム + pledge + unveil。kTLS 非対応。

seccomp はバックエンド別に許可 syscall を分割し、epoll feature 未指定時は
epoll 系 syscall を許可しない（最小権限）。

## 改修内容

- `src/runtime/` を `uring/`（既存・無変更）と `reactor/`（epoll/kqueue poller +
  readiness エグゼキュータ）に分離。公開 API パスはファサードで不変。
- build.rs が `veil_rt_uring` / `veil_rt_reactor` / `veil_poller_epoll` /
  `veil_poller_kqueue` cfg を発行。
- `src/security.rs`: seccomp 表のバックエンド分割 + FreeBSD capsicum / OpenBSD
  pledge・unveil モジュール追加。
- `docker/Dockerfile.{glibc,musl}.aarch64`、`tools/qemu/`（FreeBSD/OpenBSD VM
  ビルド・E2E ハーネス）、`tests/e2e_setup.sh` external ターゲットモード。
- `packaging/` の対象ターゲット拡張（aarch64 / FreeBSD / OpenBSD）。

## 進捗

- [x] 設計ドキュメント
- [ ] Phase 1: runtime uring 分離（無挙動変更）
- [ ] Phase 2: epoll バックエンド + seccomp 分割
- [ ] Phase 3: aarch64 クロスビルド + QEMU 確認
- [ ] Phase 4: FreeBSD（kqueue + capsicum + jail）
- [ ] Phase 5: OpenBSD（pledge + unveil）
- [ ] Phase 6: packaging
- [ ] Phase 7: 最終検証・ドキュメント
