# HTTP/3 および gRPC テストカバレッジ・セキュリティ検証レポート

本レポートは、Veil Proxy (veil-proxy-security) プロジェクトの `tests/e2e_tests.rs` および `tools/container_security/` における HTTP/3 ならびに gRPC プロトコルのテスト網羅性を調査・評価し、後続のAIエージェントがテスト拡充を行うための詳細な要求仕様（Specification）としてまとめたものです。

---

## 1. 調査概要と証拠 (Evidence)

本調査では、プロジェクト内のテスト関数一覧（421件の `test_*` 関数）と、セキュリティプローブスクリプト（`http3_probe.sh`, `grpc_probe.sh`, `grpc_web_probe.sh`）を分析しました。

### e2e_tests.rs の現状
- **HTTP/1.1, HTTP/2**: ロードバランシング、レート制限、IP制限、WASM、キャッシュ、チャンク転送、ヘッダ操作など、プロキシとしての多様な機能とエッジケースが詳細にテストされています。
- **HTTP/3**: `test_http3_basic_connection`, `test_http3_get_request`, `test_http3_proxy_load_balancing` など基本機能や性能面のテスト（34件）は存在しますが、ミドルウェア（レート制限やWASMなど）のHTTP/3経由での統合テストが欠落しています。
- **gRPC**: `test_grpc_unary_call`, `test_grpc_streaming_detailed`, `test_grpc_web_cors` など基本的なRPC通信とエラー処理（34件）は実装済みですが、コード内に `// 未実装テスト: HTTP/2ベースのgRPC詳細テスト` (`test_grpc_http2_framing`) の記述があり、またHTTP/3経由でのgRPCテストが存在しません。

### tools/container_security の現状
- **HTTP/3 (`http3_probe.sh`)**: UDP疎通確認と正常系GETリクエストのみであり、不正フレームやリソース枯渇攻撃などのセキュリティ/カオステストが含まれていません。
- **gRPC (`grpc_probe.sh`, `grpc_web_probe.sh`)**: 5バイト未満の不正ペイロード、巨大な `grpc-timeout` の送信、不正Base64など一部のパッチ検証は存在しますが、長寿命ストリームのリソース枯渇や `grpc-status` スプーフィングなど、深いプロトコル攻撃への網羅性が不足しています。

---

## 2. E2Eテスト (tests/e2e_tests.rs) 拡充仕様

AIエージェントは以下のテスト項目を `tests/e2e_tests.rs` に追加実装してください。HTTP/3およびgRPC通信が、既存のHTTP/1.1・HTTP/2と同様のセキュリティ・ミドルウェア機能の保護下にあることを実証することが目的です。

### 2.1 HTTP/3 の不足テスト要件
HTTP/3プロトコルにおいても、Veilのコア機能が透過的に機能することを証明するテストを追加します。

1. **`test_http3_rate_limiting` / `test_http3_ip_restriction`**
   - **目的**: HTTP/3接続に対してもレート制限およびIP制限が正しく適用されることの確認。
   - **手順**: HTTP/3クライアントを使用して閾値を超えるリクエストを短時間に送信し、429 Too Many RequestsがQUICストリーム上で返却されることを検証する。
2. **`test_http3_websocket` (WebSockets over HTTP/3 - RFC 9220)**
   - **目的**: HTTP/3上で拡張CONNECTメソッドを使用したWebSocket通信の検証。
   - **手順**: HTTP/3ストリーム上でWebSocketを確立し、双方向のテキスト/バイナリフレーム送受信が成立することを検証する（実装が対応している場合）。
3. **`test_http3_wasm_integration`**
   - **目的**: HTTP/3リクエスト/レスポンスに対してWASM拡張（ヘッダ書き換え・認証等）が実行されることの確認。
   - **手順**: WASMモジュールを有効化し、HTTP/3クライアントからのリクエストヘッダがWASMによって操作され、バックエンドに到達することを確認する。
4. **`test_http3_cache_hit_miss`**
   - **目的**: HTTP/3のレスポンスが正しくプロキシキャッシュに保存され、後続のQUIC接続でキャッシュヒット（304 Not Modified 含む）することの確認。
   - **手順**: キャッシュ設定を有効にし、同一URLに対してHTTP/3で複数回アクセス、2回目以降でキャッシュからの応答を検証する。
5. **`test_http3_early_data_0rtt_security`**
   - **目的**: 0-RTT（初期データ）におけるリプレイ攻撃防御の検証。
   - **手順**: べき等でないメソッド（POST等）での0-RTTリクエストが適切に拒否、または1-RTTにフォールバックされることを確認する。

### 2.2 gRPC の不足テスト要件
gRPCのトランスポート層（HTTP/2 および HTTP/3）での詳細な振る舞いを検証します。

1. **`test_grpc_http2_framing` (実装の完成)**
   - **目的**: HTTP/2フレームレベルでの不正なgRPC通信（例: DATAフレームのサイズとヘッダの不一致）のハンドリング検証。
   - **手順**: 既存の未実装部分を完成させ、不正なHTTP/2フレームを送信した際にPROXYがクラッシュせず、`RST_STREAM` 等で安全に切断することを検証する。
2. **`test_grpc_over_http3`**
   - **目的**: HTTP/3 (QUIC) をトランスポートとしたgRPC通信の検証。
   - **手順**: QUIC接続上でgRPCのUnary CallおよびStreamingを確立し、正常にデータが送受信されることを確認する。
3. **`test_grpc_client_slowloris`**
   - **目的**: 長寿命のgRPCストリームにおけるリソース枯渇（Slowloris）攻撃の耐性検証。
   - **手順**: gRPCクライアントから極めて遅いペースでフレームを送信し、プロキシのタイムアウト設定（`client_write_timeout` 等）によってストリームが適切に切断されることを確認する。

---

## 3. Container Security (tools/container_security) 拡充仕様

`tools/container_security/harness/scripts/` 以下のプローブスクリプトに、以下のプロトコル特化型の攻撃シミュレーション（カオステスト）を追加してください。

### 3.1 `http3_probe.sh` の拡充 (QUIC 攻撃・異常系)
現状の単なる疎通確認から、セキュリティプローブへと進化させます。以下を追加してください。

1. **QUIC Handshake Flood (リソース枯渇)**
   - **手法**: 無効な送信元UDPポート・IPから大量のQUIC Initialパケットを送信し、プロキシがRetryパケットを返しつつクラッシュやOOMを起こさないか検証する（`h3_handshake_flood`）。
2. **QPACK Bomb (解凍リソース枯渇)**
   - **手法**: 巨大なQPACK圧縮ヘッダ（または極端な圧縮率を持つヘッダ）を含んだHTTP/3フレームを送信し、CPUスパイクやメモリアロケーションエラーを防いでいるか検証する（`h3_qpack_bomb`）。
3. **Connection ID (CID) スプーフィング / 枯渇**
   - **手法**: 意図的に不正または無効なConnection IDを持つパケットを送信し、適切に破棄されること（既存のストリームに影響を与えないこと）を確認する。
4. **Malformed HTTP/3 Frames**
   - **手法**: 不正な長さのHEADERSフレームや、無効なストリームIDを持つDATAフレームを送信し、RST_STREAMまたはCONNECTION_CLOSEで安全に終了することを確認する。

### 3.2 `grpc_probe.sh` の拡充 (gRPC セキュリティ)
gRPCプロトコルの信頼境界（Trust Boundary）における不正入力検証を追加します。

1. **gRPC Header Spoofing (特権ヘッダの偽装)**
   - **手法**: クライアントリクエストに `grpc-status` や `grpc-message` といった、本来サーバー（バックエンド）が返却すべきトラッキング・ステータスヘッダを付与して送信する。プロキシがこれを無効化・上書きするか、またはバックエンドへそのまま通した結果異常を起こさないかを検証する（`grpc_status_spoofing`）。
2. **Oversized gRPC Message (Max Message Size 違反)**
   - **手法**: gRPCのLength-Prefixed-Messageフレームで、実際のペイロードサイズが巨大であると宣言し、プロキシが `grpc-status: 8 (RESOURCE_EXHAUSTED)` またはHTTPの413 Payload Too Largeを適切に返しメモリを保護するかを検証する。
3. **Infinite Streaming (ストリーム無制限オープン)**
   - **手法**: gRPCの双方向ストリーミングを開いた状態にし、データを全く送らずに保持し続ける（リソース保持攻撃）。指定時間が経過した後にサーバー側からコネクションが回収されることを検証する。

---

## 4. 完了条件 (Definition of Done)

次フェーズのAIエージェントは、本レポートを仕様書として以下のステップを実行し、各完了条件を満たすこと。

- [ ] **E2Eテストの実装**: `tests/e2e_tests.rs` にセクション2で定義されたすべてのHTTP/3およびgRPCテストを追加し、`cargo test` がパスすること。
- [ ] **セキュリティプローブの実装**: `tools/container_security/harness/scripts/http3_probe.sh` と `grpc_probe.sh` にセクション3で定義された攻撃・異常系シナリオを追加すること。
- [ ] **証拠の取得**: `./tests/e2e_setup.sh test` および `tools/container_security/run.sh` を実行し、追加したテスト・プローブが正常に動作し、クラッシュやエラーが発生しないこと（ログ・標準出力を証拠として提示すること）。
- [ ] **WASM / キャッシュへの結合確認**: HTTP/3・gRPCのテストが、単独プロトコルの確認だけでなく、既存の `cargo test` に組み込まれている他の機能（WASM, Prometheusメトリクス, キャッシュ）と競合・矛盾しないことを担保すること。

本レポートの指示に従い、各実装において非同期ランタイム・ゼロコピー・ブロッキング処理の排除という **「ホットパス絶対規則」** を厳格に守りつつ作業を進めてください。
