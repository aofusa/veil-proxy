# B-432: 静的配信の canonicalize の経路が、正規化したパスで封じ込めを判定した後に名前で開き直し、その間に入れ替えたシンボリックリンクをたどれた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-17）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Static-root containment checks the canonicalized path, then reopens it by name」・「open_file_cache fallback
opens the file by name after canonicalize; the route containment check runs later against the stale canonical
string」（同じ原因の 2 件）から確認。

## 事象

- `cache::resolve::fallback_open_beneath`（openat2 が `EXDEV`・`ELOOP` を返したときの再判定・openat2 の無い環境）は
  パスを `canonicalize()` してルートの下かを確かめた後、正規化したパスを `File::open` で名前で開き直していた。
- `OpenFileCache::fetch_file_info` の canonicalize の経路（Linux・FreeBSD 以外、登録されていないルート）も
  `canonicalize()` → `metadata()` → `File::open` と名前で引き直し、呼び出し側は後で正規化したパスの文字列で
  ルートの封じ込めを判定していた。
- ルートの下に書き込める者が、判定と open の間に途中の段（`d`）をルートの外を指すシンボリックリンクへ入れ替えると、
  判定は通ったまま fd はルートの外のファイルを指し、その中身を返して共有のキャッシュへも入れ得た（競合に勝つ必要がある）。

## 対応

- 正規化したパスは、設定の静的ルート（正規化した形。設定ロードのたびに記録する）から `openat(O_NOFOLLOW)` で 1 段
  ずつ開く（`resolve::open_beneath_no_symlinks`。途中の段も最後の段もシンボリックリンクならたどらずに失敗する。
  unix）。`fallback_open_beneath` はそのルートから、open_file_cache の経路はパスを含む最も深いルートから開き
  （`resolve::open_canonical`）、属性も開いた fd から取る。どのルートの下でもないパスは従来どおり名前で開く
  （呼び出し側の封じ込めの検査が断る）。
- 最後の段は `O_NONBLOCK` で開いてから外す（以前は属性を見て通常ファイルだけを開いていたので、名前付きパイプの
  open で offload のスレッドを止めない）。Windows は従来どおり。

## テスト

- `cache::resolve::tests::open_beneath_no_symlinks_refuses_swapped_components`（途中の段・最後の段のシンボリック
  リンク・ルートの外を開かず、名前付きパイプで止まらない）。既存のフォールバック経路の試験（ルートの中で完結する
  シンボリックリンクは開ける）。
