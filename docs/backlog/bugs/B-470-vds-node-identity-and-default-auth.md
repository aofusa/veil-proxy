# B-470: VDS の参照サーバがノード ID を要求の本文から取り、トークンを持つ者が他のノードの名前で NACK して配信を止められた。イメージはトークン無しで全アドレスに待ち受けていた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Node identity is taken from the request body, so any token holder can NACK on behalf of another node and block its
config delivery」と「VDS control-plane image listens on all interfaces with no authentication by default」
から確認。

## 事象

- `tools/vds/server`（Rust）・`tools/vds/vds_server.py` はノードの状態を要求の `node.id` で引き、資格情報と結び付けて
  いなかった（トークンは全ノードで共有）。他のノードの名前で NACK すると、そのノードの `rejected_version` が今の版に
  なり、後の ACK でも消えないので、まだ今の版を受けていないノード（再起動したノード等）へはスナップショットが変わるまで
  配信されなかった。
- `docker/Dockerfile.vds-server` の既定の CMD は `--listen 0.0.0.0:18000` で `--token-file` が無く、届く者は誰でも
  スナップショット（上流のアドレス・ヘッダの値）を読み、任意のノードの ACK・NACK を送れた。

## 対応

- `--node-tokens <ファイル>`（1 行に `<ノード ID> <トークン>`）: ノードごとのトークンで、要求のノード ID がトークンの
  ノードでなければ 403（gRPC は `PERMISSION_DENIED`）。`--token-file` とは排他。
- トークン（`--token-file`・`--node-tokens`）が無いときは、`--allow-unauthenticated` を付けない限りループバック以外で
  待ち受けない。イメージの既定の CMD は `--token-file /etc/vds/token`。
- 前に ACK したノードが空の版で来たら（再起動）前の NACK を持ち越さず、NACK した版を ACK したらその NACK を消す。
  まだ何も ACK していないノードの NACK は空の版の要求でも消さない（同じ版の送り直しを繰り返さない）。
- 利用者ガイド（`vds.md` の英日）に書いた。

## テスト

- `tools/vds/server` の単体試験 `node_tokens_bind_the_node_id`・`restarted_node_does_not_keep_an_old_nack`・
  `node_token_file_format`。自己完結型 E2E の VDS の試験（共有のトークン）が通ること。
- `tools/vds/server` の `cargo clippy --all-targets -- -D warnings` が通ること（リポジトリの `clippy.toml` が禁じる
  同期のファイルの読み書きは、起動時・スナップショットの監視のタスク・試験だけなので理由を付けて許した）。
