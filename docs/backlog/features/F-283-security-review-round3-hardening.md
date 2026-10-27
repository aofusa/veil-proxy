# F-283: 2026-10 のセキュリティレビュー 3 回目の Low/Info の強化項目

**状態: 未着手（2026-10-28 起票）**

## 今の状態

3 回目（`docs/artifacts/security/analysis/08`〜`11`、git 管理外）で単独のチケットにした所見（B-255〜B-258、B-246 への追記）以外の Low/Info をまとめる。

1. **GE-02 Wasm gRPC 実行器の接続タイムアウト**: `src/wasm/host/grpc_executor.rs::connect_new` はプールミス時の TCP connect + TLS ハンドシェイクを
   ゲストの `timeout_ms`（u32、上限なし）で**同期**で待つ。到達不能な上流に大きな値で呼ぶゲストがいると gRPC 実行スレッドが止まり、全プラグインの
   gRPC が進まない。`[wasm] grpc_connect_timeout_ms`（既定 5000）でクランプする。
2. **GE-03 同実行器の上限**: 受信バッファ（`WouldBlock` まで伸びる）、イベント、`GLOBAL_PENDING_GRPC_*` に上限が無い（1 メッセージ 4MB の上限のみ）。
   1 ステップで読む量と pending の件数に上限を置く。
3. **FO-01 Linkerd discovery**: `src/mesh/linkerd/discovery.rs` L89 の `RESOLVED.lock().map(..).unwrap_or(true)` は poison したロックを「解決済み」に丸める。
   `unwrap_or_else(|e| e.into_inner())` に統一する。
4. **SB-13 seccomp フィルタの固定テスト**: `build_seccomp_filter` の出力をテスト内の簡易 BPF インタプリタで実行し、`arch` 不一致・x32 番号
   （`nr | 0x40000000`）・`mmap(PROT_EXEC)`・`mprotect(PROT_EXEC)`・`pkey_mprotect` が拒否になることを固定する（現在は正しいが、許可リストの変更で
   崩れても気付けない）。`security.rs` の `build_seccomp_filter` のレイアウトのコメントで mprotect の検査が「args[1]」と書かれている箇所を「args[2]」に直す。
5. **H3-05 の文書化**: HTTP/3 は `enable_early_data()` だが h3 の接続は `is_established()` 後に作るので 0-RTT の要求の再生は起きない。
   `docs/guide/security.md`（英日）に「0-RTT の扱い」を 1 項目書く。
6. **ZC-02 の取り下げ**: `setup_ktls_after_ulp` は `CryptoInfo` を `secure_clear()` でゼロ化している。F-280 の該当項目があれば消す。

## 受け入れ条件

- 1〜4 に単体テスト。5 は文書のみ。

## 関連

- [F-282](F-282-security-review-followup-hardening.md)、[B-257](../bugs/B-257-wasm-grpc-executor-no-window-update-no-frame-split.md)
