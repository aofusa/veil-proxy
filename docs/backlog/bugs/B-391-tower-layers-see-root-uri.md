# B-391: tower の層・マウントした tokio のサービスが、`http` クレートの受け付けない文字を含む要求を `/` と見ていた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx の 2 回目のセキュリティスキャン（`CLAUDE-SECURITY-20261114-024333`）の指摘 F16 から確認。

## 事象

- `api::tower::build_http_request` は、要求先を `http::Uri::from_maybe_shared` で解析し、失敗したら `http::Request`
  の既定の URI（`/`）のままにしていた。
- veil の受け口は `!`〜`~` と 0x80〜0xFF を要求先に受け付ける（httparse の URI_MAP・`is_valid_request_target`）が、
  `http` 1.x の `PathAndQuery` は `<`・`>`・`` ` ``・UTF-8 でないバイトを受け付けない。
- `App::layer`・`App::layer_on` の層、`App::tokio_service` でマウントしたサービスは、`/admin/users/42?x=<` を `/`
  と見る。パスで認可する層は素通りし、ルータは元のパスでハンドラを選んでいた。

## 対応

- 解析に失敗したら `encode_target` で、空白・制御文字・`"`・`<`・`>`・`\`・`^`・`` ` ``・`{`・`|`・`}`・`#`・
  0x80 以上のバイトを百分率符号化して解析し直す（結果は `/` で始まる）。受け付ける要求先は変えない。
- 利用者ガイド（`library.md`・`ja`）に書いた。

## テスト

- `api::tower::tests::tower_layers_see_the_real_path_for_unusual_targets`（`--features full,api`）。
