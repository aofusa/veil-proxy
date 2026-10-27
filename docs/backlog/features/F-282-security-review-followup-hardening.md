# F-282: 2026-10 のセキュリティレビュー 2 回目（F-281 の続き）の Low/Info の強化項目

**状態: 未着手（2026-10-27 起票）**

## 今の状態

F-281 の 8 領域を読み終えた（`docs/artifacts/security/analysis/07-f281-followup.md`、git 管理外）。単独のチケットにした所見（B-244〜B-251）以外の
Low/Info をここにまとめる。優先順:

1. **KT-01 kTLS/TLS の終端**: (a) kTLS RX の有効後に TLS 1.3 の KeyUpdate・alert・NewSessionTicket が届くと `read(2)` が `EIO` になり、
   veil は「相手が閉じた」として接続を閉じる（`src/proxy/mod.rs` L272〜430 の `is_connection_closed_error`）。`recvmsg` の cmsg
   （`TLS_GET_RECORD_TYPE`）で制御レコードを受け、alert は close、KeyUpdate は rustls へ戻すか接続を閉じるのを明示する。
   (b) rustls・kTLS とも close_notify 無しの TCP EOF をきれいな EOF として扱う（`ktls_rustls.rs` L533・L576・L1185・L1341）。`Content-Length` 無し・
   EOF 終端の応答は切り詰めを検出できない。EOF 終端の応答では close_notify を要求する（無ければ本文不完全として B-17 の切断）。
2. **KT-02**: ハンドシェイク直後に rustls のデフレーマに部分レコードが残った状態で kTLS RX を有効化すると境界がずれる（`try_enable_ktls_server`・
   `drain_rustls_into`）。残りバイトがあれば kTLS RX を諦めて rustls のまま続ける。TLS 1.3 のクライアントが最後のフライトと最初のアプリケーション
   データを 1 セグメントで送るケースで実機再現する。
3. **SB-11**: namespace の `/proc`・`/dev` の mount 失敗を `warn!` だけで続行する（`security.rs` L2021・L2030）。`strict` 相当のモードでは起動を止め、
   `report_security_status` に mount の成否を出す。
4. **SB-12**: macOS Seatbelt のプロファイルは `process-exec`/`process-fork`/`network*`/`file-read*` を無条件許可（L3466〜3545）。
   `file-read*` を設定・証明書・静的ルートの `subpath` に、`process*` を deny に絞る（コメントにある将来の tightening）。
5. **WS-03**: `src/wasm/host/shared_data.rs` のキー数に上限が無い（値のサイズは検査あり）。`[wasm] shared_data_max_keys`（既定 10 万）を足す。
   CAS 指定でキーが無いときは `CasMismatch` にする（ABI どおり）。
6. **WS-04**: `http_call.rs` L148 の `timeout_ms as u32` は負値が約 49 日になる。`max(0)` + 上限（`[wasm] http_call_max_timeout_ms`）でクランプする。
7. **MS-02**: `src/mesh/identity.rs` の `san_matches` は matcher が空なら任意の SAN を許す（設計どおり）。`docs/guide/mesh.md`（英日）に
   「`san` 省略 = 認証（チェーン検証）のみで認可なし」を明記する。
8. **WB-01**: `src/proxy/websocket.rs` の 101 の後のトンネルにアイドルタイムアウトが無い。`[route.websocket] idle_timeout_secs`（既定 0 = 無制限）を足す。
9. **PY-01**: `crates/veil-python/src/native.rs` L65〜L69・L266 の `&mut HandlerContext` を GIL 解放の間も保持する。`&` + 内部可変性にする。
10. **AL-01 veil_alloc の強化**（メモリ安全性の欠陥は無し）: セグメントのヘッダがスロット 0 と同じマッピングにある（ヘッダを別マッピングか
    ガードページ付きに）、侵入型フリーリストの `next` が無保護（glibc の safe-linking 相当の XOR）、ガードページ無し。高速パスに分岐を増やさずに
    できる範囲（セグメント確保時だけ）で検討する。`fork` の子で他スレッドのヒープが孤立する（リークのみ。veil は `fork` しない）。

## 改修内容

- 上の順に 1 PR 1 項目で対応する。1・2 は kTLS の E2E（`tests/e2e_tests/ktls.rs`）に close_notify 無しの EOF・KeyUpdate のケースを足す。
- 設定キーを足すものは `examples/config.toml`・`docs/guide/`（英日）を同じ PR で更新する。

## 受け入れ条件

- 1・2: kTLS の E2E（`tests/e2e_tests/`）で、close_notify 無しの EOF で終わる EOF 終端の応答が「不完全」としてクライアントへ切断で伝わる、
  KeyUpdate を受けても接続が平文の混入なしに扱われる（閉じるなら明示のログ）。
- 3: namespace の mount に失敗したとき `strict` 相当のモードで起動が止まり、`report_security_status` に mount の成否が出る。
- 5・6・8: 新しい設定キーが `examples/config.toml`・`docs/guide/`（英日）にあり、上限超過の単体テストがある。
- 10: `tools/perf/alloc_ab.sh` の交互 A/B で 3B・54KB のスループットと `*_cpu_us_per_req` が退行しない（高速パスに分岐を足さない）。

## 依存・リスク

- 1・2 は kTLS の受信経路（`src/ktls_rustls.rs`）の変更で、kTLS でしか顕在化する差がある（B-25/B-27/B-28 の経緯）。kTLS 有効の E2E を必ず回す。
- 10 はアロケータの高速パスに触れないこと（AGENTS.md「アロケータ・メモリ」）。セグメント確保時だけに限る。

## 関連

- [F-281](F-281-security-review-remaining-areas.md)、[F-280](F-280-security-review-info-hardening.md)、[B-244](../bugs/B-244-h1-chunked-timeout-upstream-conn-pooled-mid-response.md)〜
  [B-251](../bugs/B-251-upstream-connect-failure-error-log-per-request.md)
