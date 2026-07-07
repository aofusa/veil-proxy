# B-27: kTLS + HTTP/2 高並行送信で short write によるフレーム同期破壊（FRAME_SIZE_ERROR）

- **優先度**: P1
- **状態**: 完了（2026-07-07 修正・検証済み）
- **検出**: `tools/perf`（0.5.0 リリース前ベンチ、2026-07-07）の `h2_1_ktls_1_lb_kernel_*` 構成

## 事象

kTLS 有効の HTTP/2 静的配信構成（`h2_1_ktls_1_lb_kernel_ofc_{0,1}`）に対する
`h2load -n 30000 -c100 -m10` が **256〜736 req/s**（kTLS 無効時は約 2700 req/s）まで
激減する。CPU 使用率は 6〜31% とアイドル寄りで、遅いのではなく**接続が途中で死んで
いる**（`-c10 -n1000` で 1000 中 31 リクエストのみ完了して h2load が終了する）。

veil のログにはクライアント発の GOAWAY が繰り返し記録される:

```
HTTP/2 GOAWAY received: error_code=6, last_stream_id=0, debug=too large frame size
```

従来この事象は「kernel SO_REUSEPORT の接続偏り」による特性として記録されていた
（tools/perf/README.md）。今回チケット化して原因を確定した。

## 調査（2026-07-07・原因確定）

- error_code=6 = FRAME_SIZE_ERROR。クライアントは SETTINGS_MAX_FRAME_SIZE（16384、
  h2load は変更しない）を超える「フレーム」を観測している。
- veil の `send_data` は `remote_settings.max_frame_size` を尊重して分割しており、
  フレームエンコード自体は正しい → **バイトストリームの同期が壊れて、ボディの
  中身がフレームヘッダーとして解釈されている**と推定。
- 送信経路の比較で確定:
  - rustls モード（`ktls_enabled = false`）の `write` は内部で
    `while written < len` の**全量書き込みループ**を持つ → 正常。
  - kTLS モードの `write` は**単発の io_uring SEND** に委譲するため、送信バッファ
    満杯時に **short write（部分書き込み）** が発生する。
- ところが `runtime/io.rs` の `AsyncWriteRentExt::write_all` が「簡略実装」で、
  **short write 時に残りを書かず `WriteZero` エラーを返却**していた。この時点で
  送信済みプレフィックス（フレーム途中まで）はワイヤに出ており、以降同一コネクション
  へ送信が続くとクライアント側のフレームパーサが途中バイトを長さフィールドとして
  読む → 「too large frame size」→ GOAWAY 切断。
- HTTP/2 は HEADERS+DATA を連結バッファで 1 write にまとめる（F-73）ため書き込み
  サイズが大きく（54KB 応答 + 多重化 10 ストリーム分）、高並行時に sndbuf 満杯 →
  short write を踏みやすい。HTTP/1.1 や低並行 E2E では書き込みが小さく顕在化しに
  くかった。kernel LB（4 ワーカー分散）で悪化するのは並行送信量が増えるため。

## 修正内容（2026-07-07）

`src/runtime/buf.rs` / `src/runtime/io.rs`:

- `SlicedIoBuf<T: IoBuf>`（オフセット付き `IoBuf` ラッパー）を新設。
- `write_all` を「short write 時は `SlicedIoBuf::advance()` でオフセットを進め、
  **追加アロケーションなしで残りを書き続ける**」正しいループへ書き換え
  （`Ok(0)` は WriteZero エラー、`WouldBlock` は同一オフセットで再試行）。
- `http2/connection.rs` / `http2/client.rs` の旧仕様前提コメントを更新
  （呼び出し側の不変条件「Ok = 完全書き込み」は不変）。

## 検証

- 回帰ユニットテスト 3 件（`runtime::io::tests`）: short write 継続でバイト列の
  欠落・重複がないこと、全量書き込み、`Ok(0)` の WriteZero 化。
- `h2load -n 30000 -c100 -m10`（kTLS + kernel LB）: GOAWAY(FRAME_SIZE_ERROR) が
  解消し、req/s が kTLS 無効時と同等レンジへ回復すること（結果は `docs/perf/` 参照）。

## 関連

- [B-25](B-25-reverse-proxy-http1-wrk-zero-completed.md): 同じ perf 拡充で検出された
  kTLS splice の `SPLICE_F_MORE` ハング。kTLS 送信経路の別バグ。
