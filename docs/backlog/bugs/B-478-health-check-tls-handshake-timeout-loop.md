# B-478: ヘルスチェックの TLS のハンドシェイクが時間切れで眠って繰り返し、黙る上流で全部のヘルスチェックが止まっていた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max の確認で、FreeBSD 14.3 の VM のサンドボックスの E2E（接続ブローカーの試験）の veil の
ログに panic（`panicked at library/std/src/sys/thread/unix.rs ... left: -1 right: 4`）が出たことから見つけた
（main の 92197f25 から）。

## 事象

- HTTPS・gRPC（`use_tls = true`）のヘルスチェック（`upstream::perform_health_check`・`perform_grpc_health_check`）は、
  TLS のハンドシェイクを `complete_io` で回し、`WouldBlock` なら 10ms 眠って繰り返していた（非ブロッキングのソケットを
  想定した作り）。プローブのソケット（`connect_probe`）はブロッキングで、読み書きに `timeout_secs` を掛けてあるので、
  `WouldBlock` はその時間切れだった。
- 接続は受けるがハンドシェイクに答えない上流（過負荷・止まりかけ）では、時間切れのたびに眠って繰り返し、いつまでも
  返らなかった。ヘルスチェックは全部の上流を 1 本のスレッドで順に確かめるので、他の上流のチェックも止まり、その上流は
  直前の状態（healthy）のまま残った。
- FreeBSD の capability mode では、`std::thread::sleep`（`clock_nanosleep`）が `ECAPMODE` で失敗して std の assert が
  panic し、ヘルスチェックのスレッドが終わった（以後ヘルスチェックが走らない）。サンドボックスの E2E では、ホストが高負荷で
  上流（`threads = 1` の veil）のハンドシェイクが時間切れになった回に出た。
- 接続ブローカーの同期の接続（`connect_broker::Client::connect_blocking`。ヘルスチェック・WASM の外部呼び出し・
  OpenTelemetry の送信）も、制御ソケットの送信バッファが一杯のとき `std::thread::sleep` で待っていた。ブローカーは
  capability mode のときだけ動くので、そこへ来ると必ず panic した。

## 対応

- ハンドシェイクのループは、失敗（時間切れを含む）でそのまま unhealthy を返す。
- `connect_blocking` の待ちは `server::cap_safe_sleep`（capability mode でも使える `select(2)` の時間切れ）にする。

## テスト

- `upstream::tests::test_health_check_gives_up_on_a_stalled_tls_handshake`（接続を受けてハンドシェイクに何も返さない
  上流に対し、HTTPS・gRPC のヘルスチェックが `timeout` で false を返す）。修正前のループに戻すと 20 秒待っても返らず落ちる。
- FreeBSD 14.3 の VM のサンドボックスの E2E（`bsd-vm.sh freebsd x86_64 security-e2e`）の接続ブローカーの試験で
  panic が出ない（`SECURITY_E2E=PASS`）。同じ VM で単体 1,291・統合 63・自己完結型 E2E 55 が通る。Linux は単体 1,414・
  統合 63・自己完結型 E2E 58（io_uring・epoll）・E2E 595。
- capability mode の接続ブローカーが返すソケットも、`blocking_stream` が `O_NONBLOCK` を外し、`connect_probe` が読み書きに
  `timeout` を掛けるので、`WouldBlock` は時間切れだけ（ハンドシェイクの途中の待ちで失敗にはならない）。
