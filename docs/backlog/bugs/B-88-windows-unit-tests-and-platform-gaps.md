# B-88: Windows 実機で単体テストがコンパイルできず、ディレクトリ open と shutdown 解除にも不具合があった

## 事象

これまで Windows はクロスビルド（`build-cross.sh --target windows`）でしか検証して
おらず、**`cargo test` を Windows 上で実行したことが無かった**。実機（x86_64、MSVC）で
初めて実行したところ:

1. **単体テストが 15 件のコンパイルエラー**（`std::os::unix` / `libc::socketpair` /
   `libc::rlimit` / テスト補助 `io_uring_available` の WSAPoll 版欠落）。
   非 Linux 向けにも 1 件（`udp::socket` のテストが Linux 専用定数
   `GSO_SEGMENT_SIZE` を参照し、macOS / BSD でもテストがコンパイル不能）。
2. コンパイルを通すと 3 件が失敗:
   - `cache::resolve::tests::empty_rel_opens_root_itself` — **本番の不具合**。
     `fallback_open_beneath` が `File::open` でディレクトリを開こうとするが、
     Windows の `CreateFileW` は `FILE_FLAG_BACKUP_SEMANTICS` 無しではディレクトリを
     開けず `ERROR_ACCESS_DENIED` を返す。静的ルート自身やディレクトリ URL の
     解決が PermissionDenied になる。
   - `l4::proxy::tests::test_forward_direction_wasm_shutdown_unblocks_peers_blocked_read`
     — **本番の不具合**（B-57 の Windows 版）。Linux/BSD ではローカルの `SHUT_RD` で
     待機中の poll が即座に readable になるが、Windows の WSAPoll はローカルの
     `SD_RECEIVE` を報告しない。片方向の close で対向方向の read を解除する経路が
     idle timeout（30 秒）まで止まっていた。
   - `config::shipped_config_tests::shipped_config_toml_parses_and_validates` —
     テストの問題。Windows のパス（`C:\Users\...`）をそのまま TOML の基本文字列へ
     埋め込み、`\U` がエスケープとして解釈されていた。
3. テストビルド時の警告 2 件（非 Linux で `root_b` が未読、Windows で
   `system::tests` が空になり `use super::*` が未使用）。

## 修正

- テストの Unix / Linux 専用部分に `cfg` を付ける（`allow(dead_code)` は使わず、
  `RegisteredTestRoots` は TempDir を配列で保持する形に変更）。
- `fallback_open_beneath` は Windows で `FILE_FLAG_BACKUP_SEMANTICS` を付けて開く。
- Windows reactor の `TcpStream::shutdown` は `SD_RECEIVE` / `SD_BOTH` のとき
  `wake_all_readers` で読み取り待機者を起こし、recv を再試行させる。
- shipped config テストはパスの区切りを `/` にして埋め込む。

## 教訓

B-69 / B-73 / B-81 と同じく「Linux の全テストを通過する」不具合だが、本件は
**クロスビルドでも検出できない**（ライブラリ本体はコンパイルできる。テストは
ビルドされず、実行時の挙動差はクロスビルドでは見えない）。Windows / macOS 実機での
`cargo test` を検証手順に含めること。
