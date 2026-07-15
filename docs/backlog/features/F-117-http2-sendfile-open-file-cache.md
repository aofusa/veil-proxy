# F-117: HTTP/2 File 配信の open_file_cache 対応（HTTP/1.1 との経路統一）

- 優先度: P2
- 状態: 完了
- 起点: F-116 レビュー時に発見した HTTP/1.1 / HTTP/2 の File 配信経路差

## 機能説明（事象）

HTTP/2 の File 配信（`src/proxy.rs` の `h2_sendfile`）が HTTP/1.1（`handle_sendfile`）と
異なる実装になっており、次の問題がある。

1. ルートの `open_file_cache` 設定（`Backend::SendFile` の設定値）を**無視**し、
   毎リクエストで `mime_guess` を実行する（HTTP/1.1 は `cache::get_file_info_with_config`
   で canonicalize / metadata / mime_guess をキャッシュ）。
2. ディレクトリルートで **`Path::is_dir()`（同期 metadata システムコール）を
   イベントループ上で実行**しており、ホットパス絶対規則（同期 I/O 禁止）に違反する。
3. HTTP/1.1 に存在する **canonical パスの封じ込め検査**（シンボリックリンク等で
   base_path 外へ出ていないことの確認）が HTTP/2 側に無い。

## 改修内容

`h2_sendfile` のパス解決・メタデータ取得を HTTP/1.1 `handle_sendfile` と同じ
`cache::get_file_info_with_config`（OpenFileCache。キャッシュミス時は offload で
ブロッキング解決）へ統一する。

- `h2_dispatch` の `Backend::SendFile` アームで無視している open_file_cache 設定を
  `h2_sendfile` へ引き渡す。
- パス解決 → ディレクトリならインデックスファイル解決 → canonical 封じ込め検査 →
  キャッシュ済み `file_size` / `mime_type` を使用、の流れを HTTP/1.1 実装
  （`handle_sendfile` の該当ロジック）に揃える。ファイル内容の読み込みは従来どおり
  `runtime::io::read`（offload 済み、B-26）を canonical パスに対して行う。
- `Path::is_dir()` / 毎リクエスト `mime_guess` を排除する。

## 期待効果

- ホットパス絶対規則違反（イベントループ上の同期 metadata）の解消。
- `open_file_cache` 有効時の per-request システムコール削減（canonicalize / metadata /
  mime_guess がキャッシュヒット）。
- パス封じ込め検査の HTTP/1.1 / HTTP/2 パリティ（防御強化）。

## 留意点

- `open_file_cache` 無効ルートでは、キャッシュ無し解決（canonicalize + metadata、B-14 の
  フォールバック）が従来の h2 経路（解決なしで直接 read）より per-request コストを増やすが、
  封じ込め検査の正しさを優先して HTTP/1.1 と同一挙動とする。
- 応答内容・ステータスコードの外部挙動は不変（403/404 の判定順は HTTP/1.1 準拠へ寄る）。

## 検証

- 既存 E2E の HTTP/2 静的ファイル配信テスト群 + `cargo test`
- 全 feature ビルド警告 0 / clippy / fmt
- perf: `CONFIG_GLOB='h2_1_ktls_0_lb_kernel_ofc_1'` で F-116 比の HTTP/2 非劣化〜改善を確認
  （変更は HTTP/2 File 経路のみのため HTTP/1.1 ベンチは不要）

## 検証結果

- 実装: `h2_sendfile`（`src/proxy.rs`）のパス解決を `handle_sendfile` と同じ
  `cache::get_file_info_with_config` 経由に統一。イベントループ上の同期
  `Path::is_dir()` と毎リクエスト `mime_guess` 呼び出しを排除。ディレクトリルートの
  canonical 封じ込め検査・index ファイル解決の欠如を解消（HTTP/1.1 と同一挙動）。
  `h2_dispatch` の `Backend::SendFile` アームで無視していた `open_file_cache_config` を
  `h2_sendfile` へ配線。
- `cargo fmt` / `cargo fmt --check`: 問題なし。
- `cargo build --features full`: 警告 0（2m46s）。
- `cargo build`（default）/ `cargo build --no-default-features` / `cargo build --features http2` /
  `cargo build --features "http2,grpc-full"`: いずれも警告 0。
- `cargo clippy --features full -- -D warnings`: エラー・警告 0。
- `cargo test --lib --features full`: 752 passed / 0 failed（`src/http2/connection.rs` の
  `FRAME_PING` dead_code 警告は本チケット変更前の main 由来で無関係）。
- `./tests/e2e_setup.sh test`: 530 passed / 0 failed（回帰なし）。
- perf（`h2_1_ktls_0_lb_kernel_ofc_1`、F-116 実測との比較）: HTTP/2 スループット
  非劣化〜微改善（h2load `-t4`: glibc 3646.2 → 3662.5 ± 143.5、musl 3446.5 → 3589.8 ± 58.4）、
  HTTP/1.1 不変。HTTP/2 の CPU 使用率が低下（t4: 168→159%、t1: 144→125%。
  per-request の metadata/mime 系 syscall 削減による）。詳細 `docs/perf/README.md`。
