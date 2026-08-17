# F-157: h2c 平文の対 nginx 劣後を解消する（DATA ゼロコピー案は実測で棄却）

- 優先度: P2
- 状態: **完了（対 nginx 0.51 → 0.886）。ただし当初案の「DATA フレーム writev ゼロコピー」は実測で回帰したため revert 済み**
- 関連: F-74（HTTP/2 フレーム連結）、F-116（HTTP/2 多重化アクターモデル）、F-146（静的コンテンツキャッシュ）、F-150（rustls 暗号文の writev 直結）、F-155/F-156（FreeBSD perf）
- 出典: `docs/artifacts/freebsd_h2c_perf_investigation.md`、`docs/artifacts/freebsd_h2c_l4_perf_analysis.md`

## 結論（先に要点）

h2c 平文の劣後は **memcpy 由来ではなかった**。着手条件だった「本当に memcpy 由来か」の
切り分けを DTrace で行った結果、**支配項はリクエストごとのファイル読み込みと
offload スレッドプール往復**だった。当初案（DATA フレームの writev ゼロコピー化）は
実装・検証まで行ったうえで **交互 A/B で回帰が確定したため revert** した。

| 段階 | h2c 対 nginx |
|---|---|
| 開始時 | 0.51 |
| ホットパスのコピー・確保を排除（下記 1） | 0.59 |
| 静的コンテンツキャッシュ経路の是正（下記 2） | **0.86** |
| DATA writev ゼロコピー（下記 3） | 0.81 ← **回帰・revert** |
| 最終 | **0.886** |

副次的に `h2_file_tls`（TLS の HTTP/2）が対 nginx 1.05 → **1.26〜1.38** になった。

## 切り分け（DTrace、FreeBSD 14.3 aarch64、54,576B、N=128,000 で正規化）

1 リクエストあたりの syscall:

| syscall | 改修前 | 最終 |
|---|---|---|
| `openat` | 1.00 | 0.016 |
| `fstat` | 2.00 | 0.032 |
| `lseek` | 1.00 | 0.016 |
| `close` | 1.00 | 0.016 |
| `read` | 3.00 | 0.078 |
| 1 バイト `write`（offload 完了通知パイプ） | 1.00 | 0.016 |
| `_umtx_op`（offload のスレッド間同期） | 1.58 | 0.016 |
| `poll` | 1.88 | 0.53 |
| `sendto` | 0.37 | 0.40 |
| 合計 | **約 12** | **約 1.1** |

DTrace 下の rps は 41,471 → 58,020（コピー排除）→ **80,262**（キャッシュ）と推移した。

**なぜ h2c だけが劣後していたか**: nginx は h2c でも `sendfile(2)` + `sf_hdtr` で
DATA フレームヘッダごとカーネル内で完結できる。一方 veil は HTTP/2 の DATA 再フレーミングが
必要で構造的に `sendfile` に載せられず、ファイル本体をユーザ空間へ読み出す必要がある。
TLS 版 HTTP/2 が勝っていたのは、暗号処理のコストが両者で支配的になり、この固定費が
相対的に隠れていたため（平文でマスキングが外れて露呈した）。

## 実施した改修

### 1. ホットパスのディープコピー・ヒープ確保の排除

- `build_h2_compressed_file_response` を `Bytes` で受けて非圧縮時はそのまま返す。
  呼び出し元が渡すのは F-146 の静的コンテンツキャッシュが返す `Bytes` なのに、
  直後の `to_vec()` が**ゼロコピー設計を無効化していた**。`MemoryFile` 側は
  `Bytes::from_owner` で `Arc<Vec<u8>>` を参照カウント共有する。
- `runtime/buf.rs` に `OffsetBufMut`（オフセット付き所有権ビュー）を追加し、
  `fill_read_buf` の `split_off` による毎 read の malloc + memcpy を排除。
- `drive_h2_streams` の毎イテレーション `collect::<Vec<u32>>()` を排除。
  走査バッファは**コネクションごと**に持つ（スレッドローカルにすると、このループが
  `.await` をまたぐため同一スレッド上の別コネクションが割り込んで `clear()` し、
  走査中の ID 列を壊す）。

### 2. 静的コンテンツキャッシュ経路の是正（効果が最大）

**`static_file_cache` と `open_file_cache` はセットで有効にしないと効かない。**
`cache::get_static_file_with_content`（`src/cache/static_file.rs`）の offload ゼロ経路は
「**メタデータキャッシュがヒットしたときに限り本体キャッシュを参照する**」構造のため、
本体キャッシュだけ有効にしても素通りして毎リクエスト offload の open+read に落ちる
（実測で `openat` が 1.0/req のまま変わらず、スループットも改善しなかった）。
計測ハーネス（Linux / FreeBSD 双方）を両方有効にするよう修正し、比較対象の nginx にも
同等の `open_file_cache` を入れて条件を揃えた。

### 3. DATA フレームの writev ゼロコピー化 → **実測で棄却（revert 済み）**

ピアの `SETTINGS_MAX_FRAME_SIZE` が既定 16384 なので 54KB は 4 フレームに割れる。
2 本固定の既存 `writev2` では `sendmsg` が 4 回になり memcpy 削減分と相殺されるため、
**N 本の iovec を 1 回の `sendmsg` に並べる**方式（`IoSeg` / `write_all_vectored_n` /
`pending_segs`）で実装した。実装は正しく動作し、単体 882 件・統合 54 件・reactor ビルド・
clippy すべてグリーンだった。

しかし FreeBSD aarch64 の**交互 A/B**（同一ラウンド内で base / writev を交互に測り、
それぞれ対 nginx 比で正規化）で **4 ラウンドすべて回帰**した:

| round | base | writev |
|---|---|---|
| 1 | 0.896 | 0.873 |
| 2 | 0.894 | 0.841 |
| 3 | 0.864 | 0.784 |
| 4 | 0.878 | 0.775 |

中央値 0.886 対 0.807（nginx 側は全 8 回で 109〜110k と安定＝VM が静かな良質な計測）。

**事前見積もりが誤っていた**: memcpy コストを DRAM 帯域（54KB × 92k rps ≒ 5 GB/s →
1 リクエスト 5.4µs）で見積もったが、ベンチは**同じキャッシュ済みファイルを毎回配信する**ため
コピー元は L2/L3 に residence し続け、実際の memcpy は見積もりよりはるかに安い。
一方 `sendmsg` の 8 本 iovec には per-iovec のカーネルコストが実在し、差し引きで悪化した。

> **教訓**: 「大きなコピーを消せば速くなる」は**コピー元がキャッシュに載っているかどうかで
> 結論が変わる**。ホットパスのコピー削減を検討する際は、コピー元の局所性を先に確認すること。

「コピー元が毎回異なる」ワークロード（例: 大きなプロキシ応答の中継）では再検討の余地がある。
実装は revert コミットから復元できる。

## 残件

- h2c は 0.886 で、まだ nginx を超えていない。残る差は HTTP/2 の per-request タスク
  spawn + チャネル + `Notify` 起床の固定費と見られる（`poll` 0.53/req もここに含まれる）。
  解消するには `docs/artifacts/freebsd_h2c_perf_investigation.md` の Phase 3
  「静的キャッシュヒット時の per-stream タスク spawn バイパス」が要る。
  `archive/http2-fastpath-batch`（F-131）に同種のファストパス実装があるが、
  **適用対象がメトリクス/管理 API/404/Redirect のみで静的配信を含まない**ため
  そのままでは効かない（当該ブランチ自身の A/B でも静的構成は 2470.2 → 2467.1 req/s と無変化）。
  また同ブランチはレビューでレートリミット二重消費という正確性バグが出た領域であり、
  取り込むなら静的配信への拡張と併せて慎重に設計すること。
