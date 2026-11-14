# B-396: Windows の静的配信で、ドライブ文字のパスがドキュメントルートを置き換えていた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx の 2 回目のセキュリティスキャン（`CLAUDE-SECURITY-20261114-024333`）の指摘 F25 から確認。

## 事象

- 静的配信（`proxy/sendfile.rs`・`proxy/h2.rs`・`http3_server/request_task.rs`）は `base_path.push(sub_path)` で
  パスを作る。その前に断っていたのは `..` だけで、先頭の `/` を外していた。
- Windows では `C:/Windows/win.ini`・`C:\...` は絶対パスなので `PathBuf::push` がドキュメントルートを置き換える。
  Windows には `openat2`・capsicum の経路が無いので、`fetch_file_info` はルートの外を調べ、403 と 404 の違いで
  任意のファイルの有無が見えた（内容は返らない）。代替データストリーム（`file:stream`）も同じ。

## 対応

- `http_utils::is_plain_relative_subpath` を足し、3 つの経路でファイルシステムに渡す前に確かめる（満たさなければ
  403）。Windows は `/` で始まらず `:`・`\` を含まないこと、unix は `/` で始まらないこと。
- 利用者ガイド（`security.md`・`ja`）に書いた。

## テスト

- `http_utils::tests::static_subpaths_must_be_plain_relative`（Windows の判定 `windows_plain_relative` は全 OS で回す）。
- Windows の実機では回していない。
