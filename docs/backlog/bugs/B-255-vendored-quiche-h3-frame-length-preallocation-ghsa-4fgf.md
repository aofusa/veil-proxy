# B-255: vendoring の quiche 0.24.9 が HTTP/3 のフレーム長の宣言だけでストリームごとに最大 16MB を確保し、QPACK の上限計算も誤っている（GHSA-4fgf-9xrr-88gf、CVE-2026-12523）

**状態: 未修正（2026-10-28 起票）**

## 出典

2026-10-27〜28 のセキュリティレビュー 3 回目の SC-05（`docs/artifacts/security/analysis/11-supply-chain-vendored-advisories.md`、git 管理外）。
RustSec の advisory-db と GitHub Security Advisory を直接照合して見つけた。RustSec には quiche の勧告が無く、vendoring 版の名前も `veil-proxy-quiche`
なので `cargo audit`・`cargo deny` では見えない（F-277 の背景）。

## 事象

上流の勧告（quiche 0.29.3 で修正）: HTTP/3 のフレーム解析が **宣言された長さに基づいて先にメモリを確保**し、攻撃者は宣言した分のバイトを送らなくてよい。
QPACK の `SETTINGS_MAX_FIELD_SECTION_SIZE` の計算が RFC 9114 の 1 フィールド 32 バイトの加算を含まず、広告した上限より多くのメモリを使わせられる。

veil の vendoring 版（`third_party/quiche/src/h3/stream.rs`）では:

- `state_transition` がフレーム長を `MAX_STATE_BUF_SIZE`（16MB − 1）と比べ、それ以下なら即座に `state_buf.resize(expected_len, 0)` する（L712〜716。ゼロ埋めで RSS に乗る）。
- ストリーム数の既定は `[http3] initial_max_streams_bidi = 100`、接続数の上限は無い（B-222）。ハンドシェイクを終えた 1 接続で 100 ストリーム分の宣言ができる。
- veil は `h3::Config::new()` のまま `set_max_field_section_size` を呼ばず、0.24.9 のこの検査は **フレーム全体を受け取った後**（`h3/mod.rs` L2902〜2918）。
  veil 独自の `h3_check_field_section`（B-100）はさらにその後なので、設定しても事前確保は防げない。

## 影響

- **P1**: 未認証のリモートから HTTP/3 ワーカーのメモリを消費できる（1 接続あたり宣言だけで数百 MB〜GB 規模）。HTTP/3 ワーカーは 1 本なので HTTP/3 全体の可用性に直結する。
  `http3` feature を有効にした配布物（`full`・コンテナ）が対象。

## 改修案

1. 上流の修正コミットを `third_party/quiche` へバックポートし、`README.veil.md` の差分一覧に追記する: `b732cbfa97`（h3: enforce frames size limit early）、
   `f61d62d94e`（h3: incrementally allocate state buffer for large frames）、`1e0e53767a`（h3: avoid allocating state buffer when skipping unknown frames）、
   `8215ecdd74`（qpack: fix size calculation when decoding）、`9be0e4fa18`（h3: implement a default for SETTINGS_MAX_FIELD_SECTION_SIZE。既定 32KiB）、
   `1b52c6b05d`（h3: add configurable max size for PRIORITY_UPDATE frames）。0.24.9 と 0.29.3 の `h3/` の差分を見て当てる。
2. 同じ PR で veil 側: `[http3] max_field_section_size`（既定 32KiB）を足して `set_max_field_section_size` を広告する。`examples/config.toml`・`docs/guide/`（英日）を更新。
3. 根本は quiche 0.29.3 への追従（veil の 4 差分と `veil_ffi.rs` の当て直し。0.29.3 の boringssl FFI の `c_void` 変更 #2535 に合わせる）。F-277 と一緒に。

## 受け入れ条件

- 単体（vendoring 版 quiche のテスト）: 長さだけを宣言した HEADERS フレームで `state_buf` が宣言長まで伸びないこと（上流のテストをそのまま取り込む）。
- `fuzz/` に「HEADERS の長さ宣言だけを送るストリーム」の target を足し、RSS の上限を assert する。
- `tools/container_security` の `http3` フェーズ（h3spec・h2load）が従来どおり通る。
- `cargo audit` 相当の照合で quiche の GHSA を失敗にできる（F-277）。

## 関連

- [B-256](B-256-http3-path-event-queue-never-drained-ghsa-4q5x.md)（同じ勧告照合で見つかった PathEvent）、[B-222](B-222-http3-no-connection-limit-no-retry.md)（接続数上限・Retry）、
  [B-100](B-100-http3-malformed-request-accepted.md)、[F-277](../features/F-277-vendored-quiche-advisory-matching.md)、[F-172](../features/F-172-quiche-stream-frame-coalescing.md)（vendoring の経緯）
