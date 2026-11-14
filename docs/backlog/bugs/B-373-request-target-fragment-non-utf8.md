# B-373: 要求先の `#`（フラグメント）・UTF-8 でないバイトを通し、パスの規則・RBAC を素通りできた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx のセキュリティスキャン（`CLAUDE-SECURITY-20261113-030705` の F6。MEDIUM）と、その確認の中で判明。

## 事象

- `#`: httparse・`is_valid_request_target` は要求先の `#` を通す。ルートの照合・メッシュの RBAC（完全一致・接尾辞・
  `notPaths`）は `/admin#x` を `/admin` と見ないが、上流の多く（Express・gunicorn・uvicorn）は `#` から後ろを捨てて
  `/admin` を返す（Envoy は CVE-2021-32779 の後は断る）。データプレーンの TOML のルートでも同じ（B-370 の後も
  `/admin#x` は `/admin` のルートに合わない）。
- UTF-8 でないバイト: ルートの照合は置換文字に直して読み、上流への要求・キャッシュのキーは `/` に置き換えていた
  （`from_utf8(..).unwrap_or("/")`）。別のルートの上流の `/` の応答を `/` のキーで保存できる。

## 対応

- `http_utils::request_target_is_clean`（`#` が無く UTF-8 として読める）を足し、HTTP/1.1（`origin_form_target`。
  データプレーンとメッシュ）と HTTP/2・HTTP/3（`is_valid_request_target`）で断る。HTTP/1.1 は 400、HTTP/2 はストリームの
  PROTOCOL_ERROR、HTTP/3 は H3_MESSAGE_ERROR。`path_normalization = "off"` でも断る。

## テスト

- `http_utils` の `origin_form_target_rejects_fragment_and_invalid_utf8`・`request_target_rejects_fragment_and_invalid_utf8`、
  `http3_server::tests::test_h3_field_section_rejects_smuggling_fields`。
- 文書: `docs/guide/security.md`・`configuration.md`・`routing.md`・`mesh.md`（英日）、`examples/config.toml`。
