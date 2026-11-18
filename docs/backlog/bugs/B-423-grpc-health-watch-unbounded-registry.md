# B-423: ライブラリの gRPC のヘルスの `Watch` が、クライアントの選んだ名前ごとの項目と送り口を表に入れて二度と消さなかった

**状態: 修正済み（fix/security-scan-fable-max、2026-11-17）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「gRPC health Watch keeps every client-chosen service name and watcher sender in a global map forever」から確認。

## 事象

- `App::grpc_health()`（C の `veil_register_grpc_health`）の `grpc.health.v1.Health/Watch` は、認証の無い要求の
  `service`（`grpc_max_message_size` の既定 4 MiB まで）を、知らない名前ならプロセス共有の表へ `ServiceUnknown` の
  項目として入れ、ストリームの送り口を項目の `watchers` へ積んでいた。
- 項目は消されず、送り口は `set_serving_status` がその名前に状態を置いたときにしか片付けなかった。名前を変えて
  `Watch` を開いては閉じるだけで、プロセスのメモリが際限なく増えた（可用性）。アプリが状態を置いた名前（`""` 等）も、
  状態が変わらない限り閉じたストリームの送り口が積もった。

## 対応

- `Watch` だけで作った項目（アプリが状態を置いていない名前）は、待つ相手が居なくなれば消す。表が前回の掃除の後の
  2 倍（最小 64）に達したら、閉じた送り口を外して項目を掃除する（償却で O(1)）。既にある名前へ待ちを足すときも、
  閉じた送り口を外してから積む。
- `Watch` は 1024 バイトより長い名前を `INVALID_ARGUMENT` で断る。
- `serving_status` はアプリが状態を置いた名前だけを返す（`Watch` だけの名前は `None`。`Check` は従来どおり
  `NOT_FOUND`）。
- 利用者ガイド（`library.md` の英日）に書いた。

## テスト

- `api::grpc::health::tests::watch_entries_for_unknown_names_are_swept`（閉じた `Watch` を 1 万回開いても表が
  抑えられ、生きている待ちには後から置いた状態が届く）。
