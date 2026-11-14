# B-385: `%2F`・`%5C` を含むドットのセグメント（`/public/..%2Fadmin`）がルートの制限を素通りできた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx の 2 回目のセキュリティスキャン（`CLAUDE-SECURITY-20261114-024333`）の指摘 F10 から確認。

## 事象

- TOML の経路（HTTP/1.1・HTTP/2・HTTP/3）のパスの正規化（`path_norm::normalize_request_path`）は、どのモードでも
  `EscapedSlashes::Keep` だった。`%2F`・`%5C` は残るので、`/public/..%2Fadmin` はルートの照合・
  `[route.security]` の検査では `/public/*`（または全部に一致するルート）のパスとして扱われる。
- nginx などの上流は `%2F` を `/` に戻してからドットのセグメントを取り除くので、そのまま `/admin` を返す
  （Envoy の CVE-2021-29492 と同じ類。Envoy は `path_with_escaped_slashes_action` で REJECT・UNESCAPE を選べる）。
- メッシュの経路には同じ選択肢があったが、TOML の経路では選べなかった。

## 対応

- `[security] escaped_slashes`（`EscapedSlashesMode`）を足す。
  - `keep`（既定）: エスケープしたまま送るが、エスケープされたスラッシュと隣り合うドットのセグメント（`.`・`..`。
    `%2E` も `.` と数える）があるパスは断る（正規化が有効なとき）。`%` の無いパスは `memchr` 1 回で終わる
    （ホットパスの費用を増やさない）。
  - `reject`: `%2F`・`%5C` を含むパスをすべて断る。
  - `unescape`: 正規化の前に `/`・`\` へ戻す（ルートと上流が同じパスを見る）。
- `normalize_request_path` は `Result` を返し、断る要求は HTTP/1.1 で 400、HTTP/2 でストリームの
  PROTOCOL_ERROR、HTTP/3 で H3_MESSAGE_ERROR にする。モードは `path_normalization` と同じ原子変数に詰める。
- 利用者ガイド（`configuration.md`・`routing.md`・`security.md` の英日）と `examples/config.toml` に書いた。

## テスト

- `path_norm::tests::escaped_slash_dot_segments_are_found`・`path_norm::tests::request_path_modes`。
