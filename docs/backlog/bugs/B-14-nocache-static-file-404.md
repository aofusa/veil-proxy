# B-14: `cache` feature 無効時に静的ファイル配信が 404 になる

## 出典

初回の Veil vs nginx 計測の調査中に発見（feature 別ビルド検証）。要約は `docs/perf/history.md` §2 を参照。

## 事象（再現手順）

- `cache` feature を含まない構成（例: **default features** = `ktls,http2,mimalloc`）で
  ビルドした veil で、`File` アクション（静的配信）へリクエストすると HTTP/1.1 が
  **404 Not Found** になる（HTTP/2 は 200）。
- `--features full`（`cache` を含む）でビルドすると HTTP/1.1 / HTTP/2 とも 200。

## 影響

- **P1**。`cache` を外した最小構成で `File` 配信が機能しない。フィーチャー組み合わせの
  健全性（AGENTS.md「`cfg(feature)` を壊さない」）に反する。

## 調査メモ

- `handle_sendfile`（`src/proxy.rs`）はファイル解決に `cache::get_file_info_with_config` を使う。
- `cache` feature 無効時、`src/cache/mod.rs` の当該関数は **スタブで常に `None` を返す**
  実装だった（`get_file_info` も同様）。そのため `handle_sendfile` が常に 404 を返していた。
- `cache` feature は本来「ファイル情報のキャッシュ」を追加するものであり、
  **ファイル解決そのもの** を無効化してはならない。

## 改修内容

- `src/cache/mod.rs`（`cfg(not(feature = "cache"))`）: `get_file_info_with_config` /
  `get_file_info` を、**キャッシュせず** に実ファイルを解決する実装へ変更。
  `canonicalize` + `metadata` + MIME 推測を `runtime::offload`（専用スレッド）で非同期実行し、
  イベントループをブロックしない（`cache` 有効時の `fetch_file_info` と同一設計）。

## 受け入れ条件

- [x] default features ビルドで HTTP/1.1 / HTTP/2 とも静的ファイルが 200 配信。
- [x] `cargo test --lib`（default features）が回帰なし。
- [ ] 統合テストで「`cache` なしでも File 配信が成立」を検証（同 PR で追加）。

## 関連

- B-13（seccomp faccessat2、別要因の 404）
- F-29（キャッシュのロック排除・非同期 FS）

## 対応状況: 完了（統合テスト追加のみ残）