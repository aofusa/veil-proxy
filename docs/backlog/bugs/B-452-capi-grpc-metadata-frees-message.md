# B-452: C の `veil_grpc_client_metadata` が、`veil_grpc_client_recv` で C に渡したメッセージのバッファを解放していた（解放後の使用）

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「veil_grpc_client_metadata frees the message buffer that veil_grpc_client_recv handed to C (use-after-free in
documented call order)」から確認。

## 事象

- `veil_grpc_client_recv` は呼び出しの `current`（上流の gRPC サーバの応答メッセージ）を指すポインタを返し、「次の recv
  か解放まで有効」と文書にしていた。
- `veil_grpc_client_metadata` は、終わっていない呼び出し（ストリーミング）で見つけた応答ヘッダの値を同じ `current` へ
  写していた。メッセージの `Bytes` が落ち、C が持つメッセージのポインタが解放済みの領域を指した（recv → metadata →
  メッセージを読む、という文書どおりの順で起きる）。解放した領域は他のスレッド（クライアントの要求の本文等）がすぐ使う。

## 対応

- 応答ヘッダの写しを呼び出しの別の欄（`initial`）に持ち、`veil_grpc_client_metadata` はそこを指す。応答ヘッダは届いた
  後に変わらないので、値は呼び出しの解放まで有効（届く前に呼んだときは空のまま、届いたら写す）。メッセージのポインタは
  recv と解放でだけ無効になる。
- `veil.h` と rustdoc に値の有効期間を書いた。

## テスト

- 単体の試験は実際の gRPC の呼び出しが要るので、`tests/api/library_e2e.sh` の C の gRPC クライアント（`c-kit`）で
  回帰がないことを確かめる。
