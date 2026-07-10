# F-91 失敗テスト一覧と切り分け

作成日: 2026-07-10  
実行: E2E（`cargo test --test e2e_tests --features full`、環境 `./tests/e2e_setup.sh start`）

---

## 1. 新規 F-91 E2E 結果サマリ

| テスト | 結果 | 切り分け | 対応 |
|--------|------|----------|------|
| `test_http3_rate_limiting` | **PASS** | — | — |
| `test_http3_ip_restriction` | **PASS** | — | — |
| `test_http3_wasm_integration` | **FAIL** | **実装** | [B-38](../backlog/bugs/B-38-http3-wasm-response-headers-not-applied.md) |
| `test_http3_cache_hit_miss` | **PASS** | — | body 一致で hit 相当を確認 |
| `test_http3_early_data_0rtt_security` | **PASS** | — | POST 200・プロセス生存 |
| `test_http3_websocket` | **PASS** | — | 未対応相当 502・クラッシュなし |
| `test_grpc_http2_framing_malformed_data` | **PASS** | — | 不正 length でも生存 |
| `test_grpc_over_http3` | **FAIL** | **実装** | [B-39](../backlog/bugs/B-39-http3-grpc-proxy-502.md) |
| `test_grpc_client_slowloris` | **PASS**（修正後） | 初回は **テスト**問題 | タイムアウト短縮・チャンク送信に修正済み |

---

## 2. 実装バグ詳細

### 2.1 `test_http3_wasm_integration` → B-38

- **現象**: `/wasm/` を H3 で GET → 200 だが `X-Veil-Processed` 等なし
- **根拠**: `http3_server.rs` が WASM `Continue` 時にヘッダ変更をスキップするコメント付き実装
- **テスト側**: 期待は正しい（H1 の同ルートではヘッダ付与を検証済み）。**テスト修正不要**

### 2.2 `test_grpc_over_http3` → B-39

- **現象**: H3 + `application/grpc` Unary → **502** `Bad Gateway`
- **根拠**: ログ `[HTTP/3] Async backend proxy error: Invalid HTTP response`。gRPC 上流は H2C なのに H3 経路が H1 で接続
- **テスト側**: 200 + grpc トレーラーを要求するのが正しい。**テスト修正不要**（製品未修正のため失敗継続）

---

## 3. テストケース問題（修正済み）

### 3.1 `test_grpc_client_slowloris`（初回 FAIL）

- **現象**: `ntest` 45s でタイムアウト（1 バイト遅延送信が長すぎる）
- **切り分け**: **テストケース問題**（実装 hang ではなくテストが制限時間超過）
- **修正**: 4 バイトチャンク・最大 3s 送信・その後 2s 待機・明示 close。再実行 **PASS**

---

## 4. 既知フレーキー（本作業では非対象）

- 負荷系（例: `test_concurrent_connection_stress` 等）は従来どおり触れない

---

## 5. container_security（2026-07-10 実行）

ハーネス再ビルド後、`http3_probe.sh` / `grpc_probe.sh` を Docker ネットワーク上で実行。

### 5.1 HTTP/3 プローブ

| ケース | 結果 |
|--------|------|
| http3_get | PASS |
| h3_handshake_flood (200 packets) | PASS + tls_health=200 |
| h3_qpack_bomb (StreamBlocked) | PASS + tls_health=200 |
| h3_cid_spoof (100 packets) | PASS + tls_health=200 |
| h3_malformed_frames | PASS + tls_health=200 |
| **http3: ok** | **exit 0** |

### 5.2 gRPC プローブ

| ケース | 結果 |
|--------|------|
| h2c_malformed / oversized metadata | PASS（code 返却・crash なし） |
| grpc_status_spoofing (+ TLS) | PASS |
| grpc_oversized_message (+ TLS) | PASS |
| grpc_infinite_streaming | PASS（timeout 124 + health 200） |
| **grpc: ok** | **exit 0** |

※ コンテナ設定の gRPC 上流は 404 になり得るが、セキュリティプローブの合格条件は **応答あり・プロセス生存**。

---

## 6. 未修正ポリシー

B-38 / B-39 は **チケット作成のみ**（ユーザー指示: 実装修正は行わない）。
