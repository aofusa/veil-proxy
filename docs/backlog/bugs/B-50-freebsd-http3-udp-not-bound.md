# B-50: FreeBSD で `http3_enabled = true` でも QUIC の UDP ソケットが bind されず HTTP/3 が全滅する

## 事象

FreeBSD amd64（QEMU VM、`tools/qemu/bsd-vm.sh freebsd x86_64 e2e`）で
`tests/e2e_setup.sh test` を実行すると、**HTTP/3 系のテストがすべてタイムアウト**する。

```
test result: FAILED. 416 passed; 117 failed; 0 ignored; 0 measured; 0 filtered out; finished in 801.01s
```

失敗はいずれも HTTP/3 のみで、失敗の形は一貫してクライアント側のタイムアウト:

```
---- test_http3_post_request stdout ----
thread 'test_http3_post_request' panicked at tests/e2e_tests.rs:2107:1:
timeout: the function call took 15000 ms. Max time 15000 ms

---- test_http3_pmtu_payload_sizes stdout ----
thread '<unnamed>' panicked at tests/e2e_tests.rs:19645:10:
HTTP/3 client for PMTU: TimedOut
```

HTTP/1.1・HTTP/2・gRPC・WebSocket・L4 など**それ以外は 416 件通っている**。

## 切り分け

E2E の proxy 設定では HTTP/3 が有効になっている:

```
$ grep -iE 'http3|enabled' tests/fixtures/proxy.toml
http3_enabled = true
[http3]
...
```

にもかかわらず、veil プロセスは **QUIC 用の UDP ポート（8443）を bind していない**。
起動後の `sockstat` で veil が持つ UDP ソケットは L4 UDP プロキシの 8447 だけ:

```
$ sockstat -4 -l | grep -i udp
root     veil       19586 19  udp4   127.0.0.1:8447        *:*     # ← L4 UDP プロキシ
root     test-backe 19310 14  udp4   127.0.0.1:9019        *:*
root     syslogd      711 7   udp4   *:514                 *:*
# 8443（HTTP/3）の UDP リスナが無い
```

プロキシ自体は正常に起動しており（`Proxy: OK` / TCP 8443 の HTTPS は応答する）、
**HTTP/3 のリスナだけが立ち上がっていない**。したがってクライアントは
QUIC ハンドシェイクを開始できずタイムアウトする。

## 想定原因（未確認）

- HTTP/3 の UDP データプレーンは Linux では io_uring パイプライン
  （`runtime::uring::udp_recv` / `udp_send`）、非 Linux では reactor 側の
  フォールバック経路を使う（AGENTS.md 参照）。FreeBSD（`veil_rt_reactor` +
  `veil_poller_kqueue`）でリスナ生成が行われていない、もしくは生成に失敗して
  黙って無効化されている可能性が高い。
- ビルドは通っている（`--features full-freebsd` で成功）ので、コンパイル時の
  cfg 分岐ではなく**実行時にソケットを作っていない／作れていない**側の問題と思われる。
- エラーログが残っていないため、失敗が握り潰されている可能性がある
  （まずは起動時に HTTP/3 リスナ生成の成否をログに出すところから）。

## 再現手順

```bash
tools/qemu/bsd-vm.sh freebsd x86_64 all
# もしくは既存 VM で
tools/qemu/bsd-vm.sh freebsd x86_64 e2e
```

VM 内で直接確認する場合:

```bash
tools/qemu/bsd-vm.sh freebsd x86_64 ssh \
  'cd /root/veil-proxy && VEIL_E2E_NO_DEFAULT_FEATURES=1 VEIL_E2E_FEATURES=full-freebsd \
   bash tests/e2e_setup.sh start; sockstat -4 -l | grep -i udp'
```

## 影響

- FreeBSD 向け配布物（`veil-<version>-x86_64-unknown-freebsd.tar.gz`）は
  `full-freebsd` でビルドされ **http3 を含むが、実際には HTTP/3 が機能しない**。
  README / packaging の記載と食い違うため、修正するか制限として明記する必要がある。
- HTTP/1.1 / HTTP/2 / gRPC / WebSocket / L4 は FreeBSD でも動作している（E2E 416 件通過）。

## 関連

- B-47（本件の検出につながった QEMU ビルド環境整備）
- F-120 / F-126 / F-127（FreeBSD 対応・kqueue reactor・POSIX AIO）
- F-130（HTTP/3 UDP データプレーンの io_uring パイプライン化。Linux 専用経路）
