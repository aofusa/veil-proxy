# B-124: macOS・OpenBSD で veil 本体がビルドできなかった（F-200 で消したフィールドの参照）・macOS で単体テストがビルドできなかった

**状態: 完了（feat/cross-platform-api-vds-e2e。各 OS での API の E2E 中に macOS で発見）**

## 事象

- macOS・OpenBSD で `cargo build`（`full-macos`・`full-openbsd` を含むすべての構成）が `no field 'h2c_listen' on type
  'ServerConfigSection'` で失敗した。main（1d30ebe、F-200 のマージ）以降。
- macOS で `cargo test`（単体テスト）のビルドが `SOCK_CLOEXEC`・`SOCK_NONBLOCK`・`MSG_CMSG_CLOEXEC` が無いと失敗した（F-182 以降）。

## 原因

- unveil（OpenBSD）・Seatbelt（macOS）へ渡す UDS のソケットパスを集める `config::collect_uds_socket_paths`（F-164/F-170、
  `cfg(any(openbsd, macos))`）が `[server].h2c_listen` を参照していた。F-200 で `h2c_listen` を `listen_cleartext` に
  置き換え、`listen` も `Option` にしたが、Linux では コンパイルされないこの関数だけ追随していなかった。
- FreeBSD の接続ブローカー（`connect_broker`、F-182）は「プロトコルは unix 共通」として `cfg(all(test, unix))` でも
  コンパイルしていたが、macOS には無い定数を使う（B-81 と同じ）。

## 修正

- `collect_uds_socket_paths` は `listen`・`listen_cleartext`（どちらも `Option`）を見る。`listen_cleartext` の
  `unix:<path>` も unveil・Seatbelt に渡るようになる。
- `connect_broker` の単体テストは macOS を除く unix でコンパイルする（macOS で使う場面は無い）。

## 試験

- macOS（15.5、Apple Silicon）で `cargo build --no-default-features --features full-macos --tests` が通り、
  standalone E2E・API の E2E が通過（F-203）。OpenBSD は F-203 の E2E で確認。
