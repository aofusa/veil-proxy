# F-73: HTTP/2 送信ホットパスの write_all ゼロコピー化

親: [F-24](F-24-zero-copy-pipeline-http2-http3.md) / [F-26](F-26-http2-bytes-zero-copy.md)（HTTP/2 ゼロコピー系）。

## 目的

nginx 超えに向けたボトルネック調査（`docs/perf/reports/performance_report_veil_vs_nginx_v3.md`）で、
Veil の **HTTP/2 静的配信スループットが nginx の約 71%** と最大のギャップであることを確認した。
その一因が HTTP/2 送信経路の **per-frame での二重アロケーション + 二重コピー** である。

## 現状（改修前）

`src/http2/connection.rs` の内部ヘルパ `write_all(&[u8])` は、フレームエンコード結果を
以下のように処理していた:

```rust
async fn write_all(&mut self, data: &[u8]) -> Http2Result<()> {
    let mut offset = 0;
    while offset < data.len() {
        let buf = data[offset..].to_vec();   // ← per-frame 追加アロケーション + 全コピー
        let (result, _) = self.stream.write_all(buf).await;
        ...
    }
}
```

呼び出し側（`send_data`・`send_headers_internal`・`send_response` 等 13 箇所）は既に
`frame_encoder.encode_*()` が返す **所有 `Vec<u8>`** を `&frame` として渡していた。
つまり 1 フレーム送信あたり **(1) encode_* の Vec 確保 + chunk コピー**、
**(2) write_all 内の to_vec による 2 度目の確保 + 全コピー** が発生していた。
静的ファイル（約 52KB）の HTTP/2 応答では毎リクエスト HEADERS + DATA の 2 フレームで
この二重コストを支払う（ホットパス絶対規則「ゼロコピー徹底」に反する）。

## 改修内容

`write_all` を **所有 `Vec<u8>` を受け取り io_uring stream へムーブで直接委譲** する実装へ変更。
runtime の `AsyncWriteRentExt::write_all<T: IoBuf>` は所有バッファを取り完了時に返すため、
`WouldBlock` 時は返却バッファで再試行する（runtime は「全書き込み or WriteZero」のため
`Ok` は常に完全書き込み）。呼び出し 13 箇所を `write_all(frame)`（ムーブ）へ更新。

→ per-frame の 2 度目の確保 + 全コピーを排除（1 フレーム 1 確保）。

## 計測（glibc・no_ktls_ofc・quiet host loadavg<0.5、h2load `-n30000 -c100 -m10` / wrk `-c100`、各 3 回 median）

| proto | 改修前 | 改修後 | 差分 |
|-------|-------:|-------:|-----:|
| **HTTP/2** | 1577 req/s | **1761 req/s** | **+11.6%** |
| HTTP/1.1 | 1871 req/s | 1866 req/s | ±0%（本経路を使わないため不変＝効果の切り分け確認） |

nginx 比: HTTP/2 は 75% → **84%** に改善。HTTP/2 応答ボディは改修後も sha256 一致
（バイト同一）、h2load 90,000 リクエスト全 2xx、h2spec 相当のフレーミング健全性を確認。
詳細は `docs/perf/reports/performance_report_veil_vs_nginx_v3.md`。

## 残件

- ~~`src/http2/client.rs`（proxy→バックエンド方向の HTTP/2 送信）にも同型の
  `data[offset..].to_vec()`（client.rs 内）が残る。~~ **完了（2026-07-05）**:
  `H2cClient::write_all` を所有 `Vec<u8>` のムーブ委譲へ変更し、per-frame の
  `data[offset..].to_vec()`（2 度目の確保 + 全コピー）を排除。呼び出し 15 箇所を
  ムーブへ更新（コネクションプリフェースのみ `&'static` のため接続あたり 1 回の
  `to_vec()`＝リクエストホットパス外）。モックストリームで送出フレーム列（HEADERS +
  DATA）のバイト一致とスクリプト応答の往復を検証する単体テスト
  `http2::client::tests::send_request_emits_correct_frames_and_parses_response` を追加。
- 複数 DATA フレームを 1 回の `writev`/`sendmsg` で送る scatter-gather（[F-59](F-59-writev-scatter-gather-cache.md) と同系）は
  io_uring セキュリティサーフェス拡大とのトレードオフのため別途評価。

## 受け入れ条件

- HTTP/2 送信で per-frame の余分な確保・コピーが無いこと（コードレビュー）。
- h2spec・HTTP/2 統合/E2E がグリーンで、HTTP/2 スループットが改修前を下回らないこと。
