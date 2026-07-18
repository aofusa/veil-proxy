# F-123: capsicum capability mode での静的ファイル配信（ディレクトリ fd + openat 相対化）

- 優先度: P3
- ステータス: 未着手
- 起点: F-120 Phase 4（FreeBSD capsicum 対応）

## 機能説明

FreeBSD の capability mode（`cap_enter(2)`）では絶対パスの `open(2)`/`stat(2)` が
禁止されるため、現状の静的ファイル配信（リクエスト時にパス指定で open する
`cache::get_file_info_with_config` / sendfile / memory / disk-cache 経路）は
capability mode 下で fail-closed（404）になる（F-120 Phase 4 の VM 検証で確認、
`[security] capsicum_capability_mode` の doc に制約として明記済み）。

capability mode でも静的配信を完全動作させるには:

1. 起動時（cap_enter 前）に各 File ルートのルートディレクトリを open し、
   ディレクトリ fd を保持する（`security::capsicum::limit_static_dir_rights` で
   CAP_LOOKUP/CAP_READ 等へ制限。関数は実装済み・未配線）。
2. ファイル配信経路の open/stat/canonicalize をディレクトリ fd 相対の
   `openat(2)`/`fstatat(2)` へ切り替える（パストラバーサル封じ込め検査も
   `O_RESOLVE_BENEATH`（FreeBSD 13+）で置換可能）。
3. ホットリロードで File ルートが増えた場合は capability mode では追加不可のため、
   リロード時に警告して既存 dirfd のみで継続する。

## 対象箇所

- `src/cache/mod.rs` / `src/cache/file_cache.rs`（get_file_info 系）
- `src/proxy.rs` の sendfile / memory / disk 配信経路（std::fs::read / OpenOptions）
- `src/runtime/offload.rs` 経由の canonicalize（`O_RESOLVE_BENEATH` 化で offload 不要に
  なる可能性あり）
