# B-113: connect_broker の単体テストが localhost を ::1 に解決する環境で失敗する

**状態: 完了（feat/f184-native-system-allocators）**

## 事象

`docker/Dockerfile.glibc` の builder 段の `cargo test --lib` で
`connect_broker::tests::connects_to_allowed_targets` が失敗し、コンテナイメージのビルドが止まる。

```
panicked at src/connect_broker.rs:1118:70:
called `Result::unwrap()` on an `Err` value: Os { code: 111, kind: ConnectionRefused, message: "Connection refused" }
```

ホスト上の `cargo test` では通る。F-182（接続ブローカー）で入ったテストで、F-184 の検証でコンテナビルドを
回して初めて表に出た。

## 原因

テストは上流を `127.0.0.1:0` で待ち受け、許可リストの `Target::Host("localhost:<port>")` 経由で接続していた。
ブローカーは（通常経路の `runtime::dns::resolve` と同じく）解決結果の **先頭アドレス** に接続するが、
ビルドコンテナの中では `localhost` の先頭が `::1` になるため、IPv4 だけで待ち受けている相手に繋がらない。
本体の挙動は通常経路と一致しており、壊れていたのはテストの前提（localhost は 127.0.0.1）だけ。

## 改修

テストの待ち受けアドレスを `localhost` の先頭の解決結果にした（`index_of_conn_addr` の照合も
`local_addr()` の表記で行う）。
