# B-233: HTTP/2・HTTP/3 のフィールド値の検査が C0 制御文字を通し、テキスト形式のアクセスログがエスケープしない

**状態: 修正済み（v0.10.0）**

## 出典

2026-10-27 のセキュリティレビュー（v0.9.2 の `src/http_utils.rs::is_valid_h2_field_value`・`src/access_log.rs::build_text_log` の読解）。

## 事象

- `is_valid_h2_field_value` は NUL・CR・LF と前後の空白しか拒否しない。RFC 9113 §8.2.1（HTTP/3 は RFC 9114 §4.2）は 0x00〜0x1f（HTAB を除く）と
  0x7f を禁止している。HTTP/1.1 は httparse が制御文字を拒否するが、HTTP/2・HTTP/3 では ESC（0x1b）などの C0 制御文字が上流
  （HTTP/1.1 へ変換しても CRLF 以外はそのまま）とログへ届く。
- `build_text_log` はメソッド・ホスト・パス・UA をエスケープせずに書く。端末で `tail` すると ANSI のエスケープ列が解釈される
  （画面の消去・偽のログ行）。JSON 形式は `write_json_str` でエスケープ済み。

## 改修案

- `is_valid_h2_field_value` で `0x00..=0x1f`（0x09 を除く）と `0x7f` を拒否する（HTTP/2 は `PROTOCOL_ERROR` のストリームリセット、HTTP/3 は
  `H3_MESSAGE_ERROR`）。F-236 の要求のハードニングの検査と同じ場所。
- `build_text_log` で `0x00..=0x1f`・`0x7f`・非 ASCII を `\xNN` にエスケープする（nginx の `escape=default` 相当）。ホットパスなので
  既存の書き出しバッファへ直接書く（確保を増やさない）。

## 受け入れ条件

- 単体: `\x1b[2J` を含む HTTP/2 のヘッダが拒否される。テキストのアクセスログに `\x1b` が生で出ない。
- h2spec・h3spec が変わらない。

## 修正（v0.10.0）

- `is_valid_h2_field_value` で HTAB 以外の制御文字（0x01〜0x1f・0x7f）も拒否する（HTTP/2 はストリームの PROTOCOL_ERROR、HTTP/3 は
  H3_MESSAGE_ERROR。どちらも既存の NUL・CR・LF と同じ扱い）。分岐の無い畳み込みで、値の長さに比例するだけのコスト。
- テキスト形式のアクセスログで、要求由来の値（method・host・path・upstream・user_agent）の制御文字・`\\`・0x7f 以上を `\\xNN` にする。
  印字可能な ASCII だけの値（ほとんど）はそのまま書き、確保は増えない。`docs/guide/observability.md`（英・日）に書いた。
- 単体: `h2_field_value_rejects_control_characters`・`test_text_log_escapes_control_characters`。

## 関連

- [F-236](../features/F-236-claude-security-audit.md)、[B-100](B-100-http3-malformed-request-accepted.md)
