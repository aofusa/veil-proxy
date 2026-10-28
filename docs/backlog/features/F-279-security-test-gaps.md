# F-279: セキュリティ試験の空白を埋める（seccomp 有効の E2E・HTTP/2 の接続状態機械と上流の応答の fuzz・終了時の ASAN・veil-alloc の TSAN・偽装 Initial の洪水）

**状態: 完了（v0.10.0）**

## 今の状態

既存: fuzz target 11 本（`config_toml`・`grpc_frame_decode`・`hpack_decode`・`http2_frame_decode`・`http3_frame_decode`・`http_header_validate`・
`http_request_smuggling`・`io_uring_executor`・`qpack_decode`・`wasm_abi`・`wasm_host_abi`）、`tools/container_security`（Trivy・h2spec・h3spec・testssl・
semgrep・gitleaks・ZAP・chaos・sanitizer・cargo-audit/deny・full_features の probe）、E2E 17 ファイル（`security_audit.rs` を含む）。

空白（2026-10-27 のレビューで分かった）:

1. veil 自身の seccomp・Landlock を有効にした E2E が無い（`tests/e2e_tests` に `seccomp` が 0 件）。B-219・B-220 はこれが無いために残っていた。
2. HTTP/2 の**接続状態機械**（SETTINGS の値の悪用・ストリームの列）の fuzz とメモリ上限の試験が無い（B-221）。
3. 上流の応答のパーサ（`parse_http_response` + `ChunkedDecoder`）の fuzz が無い（`http_request_smuggling` は要求側だけ）。
4. `path_norm::normalize`・`h3_check_field_section` の fuzz が無い（純粋関数で安価）。
5. HTTP/3 の偽装 Initial の洪水（B-222）に相当する負荷試験が無い（`l4_flood_probe` は TCP）。
6. graceful shutdown 中の ASAN の試験が無い（B-223・B-225）。
7. TSAN を `veil-alloc` 有効で回していない（`src/veil_alloc/` の競合は負荷試験では出にくい）。
8. `fuzz/fuzz_targets/io_uring_executor.rs` のモデルに「ガードがリングを再入する」ケースが無い（B-224）。

## 改修内容

- `tests/e2e_tests/security_audit.rs`: `enable_seccomp = true, seccomp_mode = "strict"` + Landlock で、UDS のリスナー・ディスクのスピルオーバー・
  静的配信・Wasm・HTTP/3 を 1 本ずつ通す。全スレッドの `/proc/<pid>/task/*/status` の `Seccomp:` を検査する（B-219 の修正後に 2）。
  `tests/e2e_setup.sh` で seccomp を有効にした構成を 1 つ足す（io_uring・epoll の両方）。
- fuzz target を足す: `http2_connection_state`（`Http2Connection` にバイト列を流し、動的表と本文のバッファの上限を assert）、`http_response_parse`
  （上流の応答 + chunked）、`path_norm`、`h3_field_section`。`tools/container_security/fuzz/run_libfuzzer.sh` の `FUZZ_TARGETS` にも足す。
- `tools/container_security/chaos/e2e_sanitizer_chaos.sh`（ASAN）に「HTTP/3 の受信中の graceful shutdown」「in-flight の SENDMSG を持つタスクの破棄」を足す。
  TSAN は `--features full,veil-alloc` でも回す（`container-security-nightly` の sanitizers）。
- `tools/container_security/chaos` に UDP の洪水の probe（送信元の偽装は `--cap-add NET_RAW` のコンテナで）を足し、RSS の上限を見る。
- `io_uring_executor` の fuzz のモデルにガードの再入を足す。

## 受け入れ条件

- 上の試験が `container-security-nightly` で毎晩走り、B-219〜B-225 の修正の前は失敗・後は成功する。

## 修正（v0.10.0）

- seccomp・Landlock を有効にした E2E: `VEIL_E2E_SECCOMP=strict|filter|log`（B-219/B-220 で `tests/e2e_setup.sh` に追加、`security_audit.rs` が全スレッドの
  `Seccomp:` を検査）。`container-security-nightly` の E2E の行列に seccomp `strict` を io_uring・epoll の両方で足した。
- fuzz target を 4 本足した（計 17 本）: `http2_connection_state`（`Http2Connection` にバイト列を流し、HPACK の動的表・同時ストリーム数・
  ヘッダブロックの上限を assert）、`http_response_parse`（上流の応答 + chunked を一括と 1 バイトずつで比べる）、`path_norm`、`h3_field_section`
  （`h3_check_field_section` を `src/h3_fields.rs` へ切り出し）。`run_libfuzzer.sh` の `FUZZ_TARGETS` に追加。手元で各 60 秒（66〜94 万回）落ちない。
- `io_uring_executor` の fuzz のモデルにガードの再入（detach のガードが op 表を借りる）を足し、`detach` もガードを借用の外で走らせるようにした
  （`detach_take_guard`）。借用の中でガードを走らせる変異をモデルが検出することを確認。
- `e2e_sanitizer_chaos.sh`: 負荷を流したまま TERM を送る（graceful shutdown 中の処理中の要求。HTTP/1.1・HTTP/2）。TSAN は `veil-alloc` を足して回す。
  サニタイザのビルドは既定で `http3` を含まない（build-std で quiche が重い）ので、HTTP/3 の受信中の終了は `SAN_CARGO_FEATURES` に `http3` を
  足したときだけ見る。
- 偽装 Initial の洪水: `http3-client` の `initial_flood`（別ソケットから本物の Initial を `H3_FLOOD_CONNECTIONS` 本）と `h3_flood_probe.sh`
  （洪水の後に HTTP/3・HTTPS が応答するか）、`run_full_features_security.sh` で RSS の増加の上限（`H3_FLOOD_MAX_GROWTH_MB`）。送信元の偽装（NET_RAW）は
  使わず、別ソケットで接続 ID と送信元ポートを変える。
- 受け入れ条件のうち「修正の前は失敗」は fuzz のモデルの変異でだけ確かめた（nightly で B-219〜B-225 の前のコミットを回してはいない）。

## 関連

- [F-52](F-52-cargo-fuzz-libfuzzer.md)、[F-82](F-82-fuzzing-ci-nightly.md)、[B-116](../bugs/B-116-sanitizer-chaos-leak-check-never-ran.md)、
  B-217（`run.sh` の偽 OK。feat/v0100 で修正済み）
