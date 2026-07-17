# F-121: HPACK Huffman デコードの LUT（テーブル駆動）化

- 優先度: P1
- 状態: 完了（2026-07-17）
- 起点: HTTP/2 HEADERS ホットパスの HPACK Huffman デコードが線形探索で律速し得る問題
  （v0.5.0 データプレーン性能分析）
- 設計: [docs/artifacts/hpack_huffman_lut_design.md](../../artifacts/hpack_huffman_lut_design.md)
  （Fable 設計→レビュー 3 ラウンド承認）

## 機能説明（事象）

`src/http2/hpack/huffman.rs` の `huffman_decode` は、入力ビット列に対して
`HUFFMAN_ENCODE_TABLE`（257 要素）を**シンボルごとに線形探索**し、上位ビットとの
一致をビット単位で判定していた。

- 1 シンボル確定ごとに最大 ~256 回のテーブル走査が発生し得る
- 分岐予測ミスと L1D キャッシュ汚染が HTTP/2 `HEADERS` 処理の支配的コストになり得る
- B-21 で最長符号長ガードは入っているが、正当入力でも探索コストは残る

## 改修内容（実装）

1. **4-bit LUT（第一実装）**  
   256 状態 × 16 peek、エントリはパック `u32`（`flags|sym<<8|bits<<16|next<<24`）。
   `.rodata` **16 KiB**（L1D 常駐目標）。8-bit は設計上 Phase 2。

2. **生成器**  
   `tools/gen_huffman_decode_table.py` が `HUFFMAN_ENCODE_TABLE` をパースし
   `src/http2/hpack/huffman_decode_table.rs`（`@generated`）を出力。encode 表が SoT。

3. **終端契約 I3–I6**  
   root で `bits_left < 5` または `bits_left ≤ 7` 全1 のとき NEED しない
   （pad ≥ 4 で EOS パスへ落ちる 4-bit 特有バグを回避）。`state == 0` + 0..=7 全1 で終了。

4. **公開 API**  
   `huffman_decode` / `huffman_encode` / `huffman_encoded_len` シグネチャ不変。
   `DecodeEntry` / 表は private。

## 検証結果（2026-07-17）

| 項目 | 結果 |
|------|------|
| 単体（`cargo test --features full --lib`） | **767 passed** |
| 統合 | **53 passed** |
| Huffman 専用 | 15 tests（pad 0..=7、256 ラウンドトリップ、B-21、oracle、線形極性、マイクロベンチ） |
| マイクロベンチ（release） | LUT **11.85×** vs 線形 oracle（200 ラウンド × 263 samples） |
| features ビルド（`RUSTFLAGS=-D warnings`） | no-default / default / full / 各 feature 警告 0 |
| `cargo clippy --features full -- -D warnings` | クリーン |
| `cargo fmt` | クリーン |
| E2E | 初回フル 529 passed / 2 failed（`test_concurrent_*` がホスト高負荷でタイムアウト）→ **フィルタ再実行 5/5 ok**（`test_concurrent_connection_stress` 200/200 含む） |
| tools/perf HTTP/2（scoped `h2_1_ktls_0_lb_kernel_ofc_1`） | Errors=0。h2load `-t4`: glibc http2 **1858** / http1.1 1603 / nginx 1148（nginx 比 1.62、HTTP/2>HTTP/1.1）。絶対値は co-tenant で docs/perf フルスイートより低下 |

## 受け入れ条件

- [x] 線形探索デコードを LUT 方式へ置換（公開 API 互換）
- [x] B-21 回帰・ラウンドトリップ・不正入力で panic なし
- [x] unit / integration 通過（E2E 本体 529/531、concurrent は負荷依存）
- [x] features マトリクス・clippy・fmt 警告 0、`allow(dead_code)` 不使用
- [x] tools/perf HTTP/2 + マイクロベンチで改善を記録
- [x] backlog / README（en/ja）/ AGENTS.md / docs/perf 更新

## 依存・リスク

- **依存**: なし（`http2` feature 内の純アルゴリズム変更）
- **関連**: B-21、F-26/F-116
- **再生成**: `python3 tools/gen_huffman_decode_table.py --stride 4 --huffman-rs src/http2/hpack/huffman.rs --out src/http2/hpack/huffman_decode_table.rs`
