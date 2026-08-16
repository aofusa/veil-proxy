# F-154: 静的配信の封じ込めを「ルート単位 dirfd」でカーネルに任せ、`readlink` を消す

- 優先度: P2
- 状態: 完了
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

## 検証結果（2026-08-16）

### syscall カウント（実測。本チケットの主目的の直接証明）

Linux、h2c 静的配信、`strace -f -c`。B-65 時点の計測は 1,000 リクエスト、
本計測はウォームアップ込みで約 1,200 リクエストのため、**1 リクエストあたりに
正規化**して比較する（`openat2` の実数がリクエスト数の代理指標になる）。

| syscall | B-65 時点 | F-154 後 | 1 req あたり |
|---|---|---|---|
| `readlink` | 1,012（うち errors 12） | **12（全て errors）** | 1.01 → **0.00** |
| `openat2` | 1,000 | 1,200 | 1.00 → 1.00 |
| `write`（= offload 完了通知） | 1,000 | 1,200 | 1.00 → **1.00（維持）** |
| `statx` | 2,018 | 2,418 | 2.02 → 2.02 |

**`readlink` はリクエスト単価から完全に消えた**（F-153 の成果を完全に取り戻した）。
残る 12 件は B-65 側にも**同数・同じく全件エラー**で存在する起動時の一過性プローブで
あり、リクエスト処理とは無関係。かつ **B-65 の成果（offload 1 回/リクエスト）は
維持されている**（`write` が 1.00/req のまま）ことも同時に確認できている。

計測中の 1,000 リクエストは `1000 succeeded, 0 failed` / `1000 2xx` で、
封じ込めを厳格化しても正常配信が壊れていないことを示している。

### セキュリティ上の改善（性能と同時に達成した点）

本改修は「速くなったついでに安全になった」のではなく、**封じ込めの保証形態が
事後検査から構造的保証へ変わった**点が本質:

- 変更前: `open_beneath_for_request`（**どれか**の登録済みルート）で開く
  → `readlink` で実パスを求める → `sendfile_base_contains` で**事後に**検査
- 変更後: `open_beneath_in_root(root, path)` が **その `root` の dirfd のみ**に対して
  `openat2(RESOLVE_BENEATH)` する → **そもそもルート外は開けない**

検査漏れの余地が無くなり、`readlink`（= `/proc` 依存）も不要になった。
未登録ルート・`strip_prefix` 失敗・古いカーネルはすべて `None` を返して
**`canonicalize()` + 含有チェックへフェイルクローズ**する（検査を飛ばす経路は無い）。

`containment` が `None`（any-root 探索）のまま残るのは**固定ファイルルートのみ**で、
そこでは `full_path` が config 由来の `base_path` そのもの（remainder が空でなければ
404）であり、**ユーザ入力が一切混入しない**ことを呼び出し側で確認済み。

### テスト

クロスルート拒否テスト（**最重要要件**）は、単に「失敗すること」を見るのではなく
**対照実験付き**にしてある:

1. 旧来の any-root 探索（`open_beneath_for_request`）なら root_b に**成功する**ことを先に示す
2. そのうえで `open_beneath_in_root(root_a, ...)` が root_b を**一切開けない**ことを示す
3. 上位 API（`get_static_file_with_content`）経由でも root_b の内容が配信されないこと
4. **本体が content cache に入っていないこと**（= 読んですらいない証拠）

加えて `..` 脱出・ルート外への絶対シンボリックリンク（EXDEV → フォールバック →
含有チェックで拒否）も個別に検証している。

複数ルートの登録が必要なテストは `OnceLock` の共有フィクスチャ経由に統一した。
`register_static_roots` はプロセス全体で 1 回しか有効化されないため、テストごとに
別々のルートを登録すると「単体では通るが全体実行の順序次第で落ちる」グローバル状態
依存の不安定テストになる（本ブランチで 2 回踏んだ罠と同型）ため、その再発を構造的に防ぐ。

- `cargo test --lib --features full`: **872 passed / 0 failed**（2 回連続で安定）
- `cargo test --test integration_tests --features full`: 54 passed
- ビルド（`full` / `--no-default-features` / `--no-default-features,cache`）・clippy・
  release: いずれも **warning 0**

### 付随して修正した warning

`src/cache/static_file.rs` の `use super::resolve;` は FreeBSD ビルドで未使用
（FreeBSD は `security::capsicum` 側を使うため）になり warning が出ていたため、
`#[cfg(target_os = "linux")]` で絞った。`resolve` を参照するテストも全て
`#[cfg(target_os = "linux")]` 済みであることを確認済み。`#[allow]` は使っていない。
