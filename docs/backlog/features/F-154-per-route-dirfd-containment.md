# F-154: 静的配信の封じ込めを「ルート単位 dirfd」でカーネルに任せ、`readlink` を消す

- 優先度: P2
- 状態: 対応中
- 関連: F-153（`RESOLVE_BENEATH` 封じ込め）、B-65（offload 往復の統合）

## 背景（B-65 の検証中に実測で発見）

B-65 適用後の syscall プロファイル（Linux、h2c 静的配信、約 1,000 リクエスト）:

| syscall | F-153 のみ | B-65 後 |
|---|---|---|
| `openat`（本体配信のための 2 回目の open） | 1,026 | **26**（≒ 0 回/リクエスト） |
| `write`（= offload の完了通知） | 2,000 | **1,000**（2 回 → 1 回/リクエスト） |
| `readlink` | 0 | **1,012（新規に 1 回/リクエスト）** |

B-65 は狙いどおり offload 往復と重複 open を消したが、**新たに `readlink` を
1 回/リクエスト追加してしまった**。原因は per-route の封じ込め検査で、
`openat2` で開いた fd の実パスを `/proc/self/fd/N` の `readlink` で求め、
`sendfile_base_contains` に渡しているため。

F-153 が `readlink` を 7 回 → 0 回にしたところへ 1 回戻したことになる。

## なぜ `readlink` が必要になったか

`resolve::open_beneath_for_request(full_path)` は **「`full_path` を含む
*どれか* の登録済みルート」** を探して、その dirfd 相対に `openat2` する。
`register_static_roots` は全 File ルートのパスをまとめて登録するため、
これは「そのリクエストを処理しているルート自身の配下」を意味しない。

そのため B-65 では「開いた後に実パスを求めて per-route で検査し直す」形にした
（クロスルートアクセスを防ぐため。この検査自体は必須であり削ってはならない）。

## 改修方針: 「そのルートの dirfd」に対して開く

`open_beneath_for_request` に**ルートを明示的に渡す**ようにする。

```rust
// 変更前: full_path を含む「どれかの」登録済みルートを探す
pub fn open_beneath_for_request(full_path: &Path) -> Option<io::Result<(File, Metadata)>>;

// 変更後: 「このルートの」dirfd に対して相対 open する
pub fn open_beneath_in_root(root: &Path, full_path: &Path) -> Option<io::Result<(File, Metadata)>>;
```

こうすると **`openat2(root_fd, rel, RESOLVE_BENEATH)` がそのまま per-route の
封じ込めになる**ため、

- `readlink` が不要になる（1 回/リクエスト削減、F-153 の成果を完全に取り戻す）
- 封じ込めが**構造的に保証**される（「開いてから検査し直す」ではなく
  「そもそもそのルートの外は開けない」）。検査漏れの余地が無くなる分、
  セキュリティ上もこちらが優れている

`base_path`（ルート）は `Backend::SendFile` が既に持っており、呼び出し側から
渡せる（`canonical_base` も F-145 で config ロード時に解決済み）。

## 注意

- フォールバック経路（未登録ルート・古いカーネル・その他 OS）は
  **従来どおり `canonicalize()` + `sendfile_base_contains`** を維持すること。
  そちらでは per-route 検査を省略してはならない。
- FreeBSD の `security::capsicum` 側も同じ形（ルート指定）にできるか確認すること。
  できない場合は FreeBSD だけ従来の readlink 相当の検査を残してよい（要コメント）。

## テスト

- クロスルートアクセスが拒否されること（ルート A の base_path を渡して、
  ルート B 配下のパスを開こうとすると失敗する）。**これが最重要**。
- `readlink` が増えていないことを syscall カウントで確認する（実測）。
- 既存の B-65 / F-153 のテストが引き続き通ること。

## 検証結果

（実装後に追記）
