# F-120: クロスプラットフォーム対応（epoll フォールバック / aarch64 / FreeBSD / OpenBSD）

- 優先度: P1
- ステータス: 完了（OpenBSD TLS は F-122 で解決済み）
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
- [x] Phase 1: runtime uring 分離（無挙動変更）
- [x] Phase 2: epoll バックエンド + seccomp 分割
- [x] Phase 3: aarch64 クロスビルド + QEMU 確認
- [x] Phase 4: FreeBSD（kqueue + capsicum + jail_attach、x86_64）
- [x] Phase 5: OpenBSD（ビルド + kqueue + pledge + unveil + **TLS(ring provider, F-122 で解決)**、kTLS 非対応）
- [x] Phase 6: packaging（aarch64 arch 導出 / BSD tar.gz + rc.d + jail.conf）
- [x] Phase 7: 最終検証・ドキュメント

## Phase 7 最終検証結果

- **ビルドマトリクス warning 0**: default / no-default-features / full / full,epoll、
  および各 feature 単体（ktls・http2・http3・wasm・grpc-full・compression・cache・
  metrics・websocket・rate-limit・buffering・opentelemetry・admin・access-log・
  l4-proxy・jemalloc・system-allocator・epoll）すべて warning 0（`#[allow(dead_code)]`
  不使用）。
- **clippy**: `--features full` / `--features "full,epoll"` ともに 0。
- **cargo fmt**: 差分なし。
- **テスト**: lib 758 / integration 53 / runtime_cancellation 2 すべて通過。
- **E2E**: io_uring（デフォルト）531/531・epoll 531/531 全通過。
- **BSD/aarch64**: FreeBSD VM で lib 449 通過・serving 200、aarch64 QEMU で E2E 530/531、
  OpenBSD は build/kqueue/pledge/unveil/TLS(ring, F-122) 動作・静的配信/プロキシ HTTPS 200。

## Phase 5 の記録（OpenBSD 7.9 VM 検証）

### 状態: 完了。build/kqueue/pledge/unveil/TLS(ring, F-122) すべて動作・HTTPS 200

| 項目 | OpenBSD 7.9 amd64 | 備考 |
|---|---|---|
| ビルド（http2,mimalloc / + http3,grpc-full,compression,cache,metrics,websocket,rate-limit,buffering,admin,access-log,l4-proxy） | ✅ warning 0 | libclang パス（`LIBCLANG_PATH=/usr/local/llvm19/lib`）が bindgen に必要。VM は datasize-cur=1536M のため rustc がメモリ超過で killed → `ulimit -d unlimited`・`CARGO_PROFILE_DEV_DEBUG=0`・nohup ではなく tmux で回避（nohup は ssh セッション終了時の SIGTERM を防げない） |
| kqueue reactor（accept/read/イベント配送） | ✅ | ktrace で accept4→kevent 配送→read(ClientHello 1519B) を確認 |
| pledge / unveil | ✅ | 起動時 `unveil: locked` / `pledge: promises restricted to "stdio rpath wpath cpath inet dns flock"` を確認 |
| kTLS | ❌（設計どおり） | OpenBSD は kTLS 非対応。simple_tls（ユーザ空間 rustls）へフォールバック |
| **TLS ハンドシェイク（HTTPS 応答）** | ✅（F-122） | rustls の暗号プロバイダを OpenBSD 限定で ring へ切替（aws-lc-rs は OpenBSD で未完のため）。静的配信/プロキシとも HTTPS 200、pledge+unveil 有効のまま動作 |

- **memfd フォールバック**: `memfd_create(2)` は OpenBSD に無いため、HTTP/3 証明書ホット
  リロードの `create_memfd_for_pem` を OpenBSD では 0600 の一時ファイル（Drop で unlink）へ
  フォールバック（Linux/FreeBSD は memfd + /proc/self/fd を維持）。
- **SO_REUSEPORT**: OpenBSD は SO_REUSEPORT_LB 非搭載のため `not(freebsd)` 分岐の
  SO_REUSEPORT を使用（既存コードでカバー）。
- **VM 検証の教訓**: nohup+`&` は OpenBSD で ssh セッション切断時に SIGTERM を受けて
  ビルドが死ぬ。tmux セッションで実行すると独立して完走する。ソース同期は rsync だが
  `-a`（mtime 保持）だと VM のキャッシュ済み .rlib より古い mtime で cargo が再コンパイルを
  スキップするため、確実な再ビルドには VM 側で `touch src/**` するか mtime に注意。

## Phase 3 の知見（QEMU user-mode 検証）

- **qemu-aarch64（user-mode）は io_uring 系 syscall（425-427）未実装（ENOSYS）**。
  QEMU での動作確認は `--features full,epoll`（epoll バックエンド）ビルドで行う。
  実 aarch64 ハードウェアではデフォルト（io_uring）ビルドをそのまま使える。
- **コンテナ seccomp プロファイルは QEMU ではエミュレータ自身を制限する**ため、
  veil 用最小 allowlist だと qemu 必須 syscall（membarrier 等）まで塞ぎ即 abort。
  QEMU 実行時は Docker デフォルトプロファイルを使い、カスタムプロファイル
  （`SCMP_ARCH_AARCH64` 宣言を追加済み）は実 aarch64 ホスト用とする。
- アプリ内 seccomp/Landlock も qemu が該当 syscall をエミュレートしないため適用不可
  （QEMU 検証時は無効化 or `allow_security_failures = true`）。
- **E2E 実測（QEMU user-mode、glibc-aarch64/musl-aarch64 コンテナ）**: フル E2E 約 90 秒で
  **530/531 通過**。`test_concurrent_connection_stress`（200 並行 TLS・5s タイムアウト）のみ
  エミュレーションのスループット不足で恒常失敗するため QEMU 合格基準から除外
  （ネイティブ x86_64 epoll ビルドは同テスト含め 531/531）。単体直接実行
  （`qemu-aarch64-static veil-musl-aarch64 --config ...`）でも HTTPS 200 を確認。
- musl-aarch64 イメージは `docker build --platform linux/arm64` でビルドすること
  （イメージメタデータを arm64 で記録するため。Dockerfile 内コメント参照）。
- messense/rust-musl-cross:aarch64-musl は同梱 rustc 1.88 が wasmtime の MSRV(1.89) を
  満たさず、`rustup update stable` もイメージ側の docs 削除で失敗するため、
  バージョン固定 `rustup toolchain install 1.89.0` を並置。aws-lc-sys の bindgen には
  `BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_linux_musl` でクロス sysroot 指定が必須。

## Phase 4 の記録（FreeBSD 14.3 VM 検証）

### OS 別 feature 対応表（FreeBSD x86_64、warning 0 でビルド確認）

| feature | FreeBSD | 備考 |
|---|---|---|
| http2 / http3 / grpc-full / wasm / opentelemetry / compression / cache / metrics / websocket / rate-limit / buffering / mimalloc / admin / access-log / l4-proxy | ✅ | ktls を除く全機能セットで warning 0 |
| ktls | ❌（設計どおり） | kTLS は Linux 専用（`veil_ktls` cfg）。非 Linux は simple_tls（ユーザ空間 rustls）へ自動フォールバック |
| epoll feature | ❌（build.rs でエラー） | BSD は kqueue が自動選択 |

- HTTP/3 は `udp/socket.rs` の**非 Linux 逐次エミュレーション**（recvmmsg/sendmmsg/GSO API と
  同一シグネチャを単発 sendto/recvfrom で提供、GSO/GRO 無効）で http3_server.rs 無変更のまま対応。
- UDP の SO_REUSEPORT_LB（FreeBSD、カーネル分散）は TCP リスナーと同方針で適用。
- 実機検証（VM 14.3-RELEASE / rustc 1.96.1）: 静的配信 HTTPS/HTTP2 200、
  veil→veil プロキシ経路 200、capsicum rights-limited で全経路動作、
  capability mode（オプトイン）は barrier 後 cap_enter 成功・accept/TLS 動作・
  パス open は fail-closed 404（F-123 参照）。lib テストは VM で実行。
- FreeBSD **aarch64** は rustc Tier 3（クロス不可）+ ホストに qemu-system-aarch64 が無く
  未検証（コードは target_os = "freebsd" で arch 非依存。ビルド手順は amd64 と同一の
  VM 内ネイティブビルド）。

### Phase 4 で発見・修正したバグ（VM 検証で検出）

1. **cap_enter がワーカー bind 前に適用され全ワーカー Bind error**: capability mode では
   `bind(2)` 禁止のため、全 TLS ワーカーの listener bind 完了を AtomicUsize バリアで
   待ってから cap_enter する方式へ変更。あわせて capability mode を独立キー
   `capsicum_capability_mode`（オプトイン）に分離。
2. **accept 済み接続 fd の rights 不足で TLS handshake が ENOTCAPABLE**: FreeBSD では
   accept された fd は**リスナーの rights を継承**するため、リスナー rights に
   CAP_READ/CAP_WRITE/CAP_SHUTDOWN を含める必要がある（per-accept の cap_rights_limit
   追加はホットパス syscall 増のため行わない）。
3. wasmtime の MSRV / bindgen sysroot 等のビルド環境問題（Phase 3 の musl と同種）。

## Phase 2 で発見・修正したバグ

readiness モデル移行で顕在化した 4 件（レビュー・E2E デバッグで検出、いずれも修正済み）:

1. **EPOLLONESHOT の ADD/MOD 判定**: oneshot 発火は interest を無効化するだけで監視対象
   リストから fd を除去しない。`armed == 0` を ADD/MOD 判定に使うと EEXIST → 再武装不能で
   accept ループがハング。`known_to_kernel` フラグで判定（実装エージェントが検出・修正）。
2. **Connect の spurious wake 誤判定**: `timeout(CONNECT_TIMEOUT, connect)` はタイマー起床でも
   内側 Future を再 poll する。登録済み＝起床＝完了と見なして SO_ERROR（未完了時は 0）だけを
   見ると **未接続ソケットを接続成功として返す**。POLLOUT 確認後に SO_ERROR を読むよう修正
   （レビューで検出）。
3. **offload 共有 eventfd の通知横取り**: eventfd カウンタは全 completions 合算の 1 本の
   レベル信号のため、あるタスクの try-first drain が他タスク宛てシグナルを消費すると、
   EPOLLONESHOT イベントは epoll_wait 時点のレベル再評価で蒸発し、待機タスクは done=true
   でも永久に起床しない（io_uring は POLL_ADD 完了が write 時点で CQE 記録されるため免疫）。
   done 検査つき専用 Future（`OffloadWait`）+ drain 者による `wake_all_readers` 再配布で修正
   （E2E `test_f62_wasm_http_call_concurrent_requests` のハングをトレースで特定）。
4. **wasm tick スレッドの wasmtime API 誤用（既存バグ）**: async store（fuel_async_yield）に
   同期 `TypedFunc::call` を使用しており "must use call_async with async stores" panic で
   tick スレッドが死亡、取得済み pending HTTP コールが失われる。uring ではインライン解決が
   常に先行するため潜在化していたが、epoll のタイミングで顕在化。`call_async` へ修正。
