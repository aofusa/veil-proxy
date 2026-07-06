# B-26: イベントループ上に残存していた同期 FS 呼び出し（HTTP/3 sendfile・runtime::io・ディスクキャッシュ）

## 事象

F-88 の AST ベース静的解析（clippy `disallowed-methods`）導入により、ホットパス絶対規則
（AGENTS.md）違反となる **イベントループ上の同期 FS 呼び出し** が 3 系統検出された。

1. **HTTP/3 静的ファイル配信** `src/http3_server.rs::handle_sendfile`:
   H3 イベント処理（`handle_request`）から呼ばれる同期関数内で `std::fs::read` により
   ファイル全量を読み込んでいた。大きなファイル・遅いディスクでワーカースレッドの
   イベントループ全体（同スレッドの全接続・全バックエンドタスク）が停止する。
2. **`src/runtime/io.rs::read` / `remove_file`**: 「非同期版」を名乗る互換 API が実体は
   イベントループ上の同期 `std::fs` だった。呼び出し元の `proxy.rs`（memory モードの
   静的ファイル配信）は「io_uring による非同期 I/O でブロックしない」というコメントと
   裏腹に実際はブロックしていた。
3. **ディスクキャッシュ** `src/cache/disk.rs::async_io::{read_file, write_file}`:
   `use_async_io` 有効時の「非同期」経路が実体はイベントループ上の同期 `std::fs::read` /
   `File::create` + `write_all` + `sync_all`（+ 呼び出し元の `create_dir_all`）だった。
   キャッシュのディスク層ヒット/書き込みはデータプレーン経路のため、ディスク I/O の
   レイテンシが同スレッドの全接続に波及する。

## 影響

- 対象経路のリクエストが遅いストレージと重なると、同一ワーカースレッド上の**無関係な
  接続まで**応答が停止する（thread-per-core のためスレッド単位で全滅）。
- 攻撃者が大きな静的ファイル・キャッシュミスを狙って多数リクエストすることで
  スループットを大幅に低下させられる（DoS 面）。

## 調査メモ

- 検出手段: F-88 で導入した `clippy.toml` の `disallowed-methods`（`std::fs::read` 等）。
  semgrep（F-64）は正規表現ベースのため本違反を検出できていなかった。
- HTTP/1.1 経路（`proxy.rs`）の buffering / ディスク読み込みは F-29/F-42 で offload 化
  済みだったが、上記 3 系統が取り残されていた。

## 改修内容（修正済み・2026-07-06）

- `handle_sendfile`（HTTP/3）を async 化し、whole-file read を
  `runtime::offload::offload`（専用スレッドプール + eventfd 完了待機）へ退避。
- `runtime::io::read` / `remove_file` を offload 実装へ変更（リング未初期化の
  コンテキスト＝単体テスト等では従来どおり inline 同期実行にフォールバック）。
- `cache/disk.rs::async_io::{read_file, write_file}` を offload 化し、親ディレクトリ作成
  ・書き込み・fsync を閉包内へ集約（呼び出し元 `write_async` の同期 `create_dir_all` を除去）。
- 残存する明示同期 API（`read_sync` / `write_sync` = `use_async_io=false` の明示同期モード・
  非 Linux フォールバック）は理由付き allow で現状維持（Linux 本番経路は offload 経由）。

## 関連

- 出典: [F-88](../features/F-88-ast-hotpath-blocking-lint.md)（AST lint 導入）
- 同型の既修正: F-29 / F-42（offload 化）
