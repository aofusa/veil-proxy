# B-494: `[route.cache]` の保存先の設定（`max_memory_size`・`disk_path`・`max_disk_size`・`memory_threshold`）が効かない

**状態: 未着手（2026-11-22 起票。F-257・B-491 の作業で見つけた）**

## 事象

- データプレーンは起動時に作る全体のキャッシュのマネージャー 1 つ（`entry.rs`。メモリ 100 MB・`disk_path = None`）へ
  保存する。ルートの `max_memory_size`・`disk_path`・`max_disk_size`・`memory_threshold` はそのマネージャーに渡らない。
  - ディスクのキャッシュは一度も使われない（`disk_path` を設定しても大きい応答はメモリへ置く）。
  - メモリの上限は全ルートで 100 MB を共有する（ルートの `max_memory_size` を大きくしても増えない）。
  - ルートの `max_memory_size` は、保存する応答 1 つの取り込みの上限（最大 10 MB）にだけ使う。
- 文書（`compression-cache.md`）・`examples/config.toml` は、これらの設定がルートごとに効く書き方だった
  （「Disk Cache: monoio の非同期 I/O」等）。2026-11-22 に今の挙動を書いた（F-257 の文書の更新と一緒に）。
- 一方でサンドボックス（`config::sandbox_paths`）は、ルートの `disk_path` を Landlock の読み書きの許可へ足している。

## 案

- 保存先の設定の組（`max_memory_size`・`disk_path`・`max_disk_size`・`memory_threshold`）ごとにマネージャーを作り、設定の
  読み込みでルートの解決済みのフィールド（`#[serde(skip)]`）へ `Arc` で持たせる（要求ごとはルートの `Arc` を引くだけ）。
  リロードでは同じ組のマネージャーを引き継ぐ（エントリを捨てない）。
- 引き当て・保存・裏の取り直し・stale-if-error・管理 API の PURGE・掃除・メトリクスを、全体のマネージャーからルートの
  マネージャー（掃除・PURGE・メトリクスは全マネージャー）へ替える。
- ディスクの書き込み・読み出しはプラットフォームのサンドボックスに合わせる: FreeBSD の capability mode では `disk_path` を
  起動時に開いた dirfd からの `openat` にする（静的配信の F-123 と同じ）、OpenBSD は unveil、リロードで新しく現れた
  `disk_path` は Landlock の許可に無いので、起動時に無かったパスは警告してメモリだけにする。
- 試験: ルートごとのメモリの上限、`memory_threshold` を超える応答がディスクから返ること（4 プロトコル）、リロードで
  エントリが残ること。

## 関連

- [B-491](B-491-proxy-cache-ignores-route-store-settings.md)・[F-257](../features/F-257-proxy-cache-h2-h3.md)
