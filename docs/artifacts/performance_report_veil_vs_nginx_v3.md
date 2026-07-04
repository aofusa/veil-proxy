# Veil vs nginx パフォーマンスレポート v3 — HTTP/2 送信ゼロコピー最適化

v2（`performance_report_veil_vs_nginx_v2.md`）で「Veil の HTTP/2 が nginx の約 75〜79%」と
最大ギャップだった点を、**ボトルネック調査 → コード最適化 → A/B 計測** の 1 サイクルで改善した記録。
関連チケット: [F-73](../backlog/features/F-73-http2-send-zerocopy-writeall.md)。

## 1. 計測環境

- ホスト: Linux x86_64, 4 論理コア。**quiet（loadavg < 0.5）** で計測（v2 の負荷フレーキー回避）。
- コンテナ間通信（Docker network）。TLS: 自己署名 ECDSA secp384r1。ペイロード: `docker/assets/www/index.html`。
- ツール: HTTP/1.1 = `wrk -t4 -c100 -d12〜15s`、HTTP/2 = `h2load -n30000 -c100 -m10`。
- 対象: `nginx:alpine`（`access_log off`）、`veil:glibc`（`ktls,http2,mimalloc,cache`、config=`no_ktls_ofc`）。
- Veil / opt はいずれも同一 feature・同一 config で A/B（コード差分のみ）。各 3 回の median。

## 2. ベースライン（改修前、quiet host）

| target | HTTP/1.1 req/s | HTTP/2 req/s |
|--------|---------------:|-------------:|
| nginx | 2054 | 2100 |
| veil:glibc (no_ktls_ofc) | 1819〜1875 | 1577 |
| **veil / nginx** | **~89%** | **~75%** |

→ **HTTP/2 が最大の伸びしろ**。HTTP/1.1 は tail latency（p99 70ms vs nginx 105ms）でむしろ優位。

## 3. ボトルネック分析

`perf` はホスト `perf_event_paranoid` の制約で不可、io_uring のため `strace` も
バッチ化された `io_uring_enter` しか見えない。そこで **HTTP/2 送信ホットパスのコード精査** を実施。

`src/http2/connection.rs` の内部 `write_all`:

```rust
async fn write_all(&mut self, data: &[u8]) -> Http2Result<()> {
    while offset < data.len() {
        let buf = data[offset..].to_vec();   // ← per-frame 追加確保 + 全コピー
        let (result, _) = self.stream.write_all(buf).await;
        ...
```

呼び出し側（`send_data`・`send_headers_internal` 等 13 箇所）は既に
`frame_encoder.encode_*()` が返す**所有 `Vec<u8>`** を渡していた。つまり 1 フレームあたり
**encode で 1 回 + write_all の to_vec で 2 回目**の確保 + 全コピーが発生。
静的ファイル（約 52KB）の HTTP/2 応答では毎リクエスト HEADERS+DATA の 2 フレームで
この二重コストを支払っていた（ホットパス絶対規則「ゼロコピー徹底」違反）。

## 4. 改修（F-73）

runtime の `AsyncWriteRentExt::write_all<T: IoBuf>` は所有バッファを取り完了時に返す。
`write_all` を **所有 `Vec<u8>` を受け取り stream へムーブで直接委譲**する実装へ変更し、
`WouldBlock` 時は返却バッファで再試行（runtime は「全書き込み or WriteZero」）。
13 呼び出しを `write_all(frame)`（ムーブ）へ更新。→ **per-frame の 2 度目の確保 + 全コピーを排除**。

## 5. A/B 計測結果（改修前 veil:glibc vs 改修後 veil:glibc-opt、各 3 回 median）

| proto | 改修前 | 改修後 | 差分 | vs nginx |
|-------|-------:|-------:|-----:|---------:|
| **HTTP/2** | 1577 | **1761** | **+11.6%** | 75% → **84%** |
| HTTP/1.1 | 1871 | 1866 | ±0% | 89%（不変） |

- HTTP/1.1 が不変なのは本経路（HTTP/2 send）を通らないため。**効果が HTTP/2 に限定**されることを確認。
- 正当性: 改修後の HTTP/2 応答ボディは source と **sha256 一致（バイト同一）**、
  h2load 90,000 リクエスト全 2xx、HTTP/2 unit 51 件パス。

## 6. 残る最適化余地（backlog）

- `src/http2/client.rs` の proxy→バックエンド方向にも同型 `to_vec()` が残る（F-73 残件）。
- 複数 DATA フレームの `writev`/`sendmsg` scatter-gather 集約（[F-59](../backlog/features/F-59-writev-scatter-gather-cache.md) と同系、io_uring サーフェスとのトレードオフ）。
- v2 の知見（コンテナでは kTLS よりユーザ空間 rustls 有利、cbpf は単一 IP 負荷で 1 ワーカー集約）は据え置き。
- TLS ハンドシェイク/レコード層・ルーティングの CPU プロファイル（perf 可能な環境で flamegraph 取得）。

## 7. まとめ

セキュア構成（seccomp+Landlock、config=no_ktls_ofc）を保ったまま、ゼロコピー原則に沿う
1 つの局所最適化で **HTTP/2 スループットを +11.6%（nginx 比 75%→84%）** 改善した。
HTTP/1.1 は既に nginx の ~89% + tail latency 優位。次サイクルは client.rs 送信経路と
TLS/ルーティングの CPU プロファイルを対象とする。
