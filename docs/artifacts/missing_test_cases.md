# 不足テストケース一覧（作業開始時点）

作成日: 2026-07-10  
照合: `required_test_cases.md` × `implemented_test_cases.md` × `test_coverage_report.md`

本一覧のすべてを E2E および/または container_security に実装する（F-91）。

---

## 1. E2E 不足（HTTP/3）

| ID | 実装対象 | 実装先 | 備考 |
|----|----------|--------|------|
| E-H3-10 | `test_http3_rate_limiting` | `tests/e2e_tests.rs` | `/rate-limited/*` に H3 連打 → 429 |
| E-H3-11 | `test_http3_ip_restriction` | 同上 | `/api/ip-restricted/*` → 403 |
| E-H3-12 | `test_http3_wasm_integration` | 同上 | `/wasm/*` + ヘッダ検証 |
| E-H3-13 | `test_http3_cache_hit_miss` | 同上 | `/cached/large.txt` 2 回 |
| E-H3-14 | `test_http3_early_data_0rtt_security` | 同上 | 非べき等 POST の 0-RTT 扱い |
| E-H3-15 | `test_http3_websocket` | 同上 | RFC 9220 拡張 CONNECT（未対応なら安全失敗） |

## 2. E2E 不足（gRPC）

| ID | 実装対象 | 実装先 | 備考 |
|----|----------|--------|------|
| E-G-05+ | `test_grpc_http2_framing` 強化 | `tests/e2e_tests.rs` | 不正 length-prefix / 短い DATA で panic なし |
| E-G-06 | `test_grpc_over_http3` | 同上 | H3 + application/grpc Unary |
| E-G-07 | `test_grpc_client_slowloris` | 同上 | 極遅送信 → タイムアウト/切断・生存 |

## 3. container_security 不足（HTTP/3）

| ID | 実装対象 | 実装先 | 備考 |
|----|----------|--------|------|
| S-H3-02 | `h3_handshake_flood` | `http3_probe.sh` + `http3-client` mode | Initial 洪水 |
| S-H3-03 | `h3_qpack_bomb` | 同上 | 巨大ヘッダ |
| S-H3-04 | `h3_cid_spoof` | 同上 | 不正 CID |
| S-H3-05 | `h3_malformed_frames` | 同上 | 不正フレーム |

## 4. container_security 不足（gRPC）

| ID | 実装対象 | 実装先 | 備考 |
|----|----------|--------|------|
| S-G-05 | `grpc_status_spoofing` | `grpc_probe.sh` | リクエストに grpc-status |
| S-G-06 | `grpc_oversized_message` | 同上 | 巨大 length 宣言 |
| S-G-07 | `grpc_infinite_streaming` | 同上 | 無データ保持 |

## 5. 補助実装

| 項目 | 内容 |
|------|------|
| `tests/common/http3_client.rs` | レスポンスヘッダ返却 API |
| `tools/.../http3-client.rs` | `HTTP3_MODE` 攻撃モード |
| `.gitignore` | `docs/artifacts/*.md` 追跡、実行時一時ファイル除外維持 |

## 6. 解消判定

- [ ] 上記全 ID がコード上に存在
- [ ] E2E / container_security を実行
- [ ] 失敗はテスト修正 or bugs チケット化
- [ ] 本ファイルのチェックを更新
