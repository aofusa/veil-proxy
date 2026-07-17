# F-121: HPACK Huffman デコードの LUT（テーブル駆動）化

- 優先度: P1
- 状態: 進行中
- 起点: HTTP/2 HEADERS ホットパスの HPACK Huffman デコードが線形探索で律速し得る問題
  （v0.5.0 データプレーン性能分析）

## 機能説明（事象）

`src/http2/hpack/huffman.rs` の `huffman_decode` は、入力ビット列に対して
`HUFFMAN_ENCODE_TABLE`（257 要素）を**シンボルごとに線形探索**し、上位ビットとの
一致をビット単位で判定している。

- 1 シンボル確定ごとに最大 ~256 回のテーブル走査が発生し得る
- 分岐予測ミスと L1D キャッシュ汚染が HTTP/2 `HEADERS` 処理の支配的コストになり得る
- B-21 で最長符号長ガードは入っているが、正当入力でも探索コストは残る

RFC 7541 Appendix B の固定 Huffman 符号は静的であるため、**コンパイル時に構築する
8 ビット LUT（状態遷移テーブル）**へ置き換え、デコードを「バイト単位の配列参照 +
ビットシフト」に縮約できる。

## 改修内容

1. **DecodeEntry + 多状態 LUT**  
   各状態 × 256 エントリで「確定シンボル / 消費ビット数 / 次状態 / エラー」を保持。
   テーブルは `const` 静的配置（build.rs 生成または `const` 計算）。L1 に収まる
   サイズ（状態数 × 1KB 前後）を目標とする。

2. **`huffman_decode` のホットパス刷新**  
   64bit bit-cache + 上位 8bit インデックスで LUT を引き、確定時に出力へ push。
   EOS パディング検査・不正符号・B-21 の panic 回避を維持。

3. **ホットパス規則との整合**  
   リクエストごとの追加ヒープ確保は避け、出力 `Vec` の `with_capacity` は現状維持
   または改善。テーブルは静的（実行時アロケーションなし）。

4. **検証**  
   既存ラウンドトリップ / B-21 回帰テストを維持・拡充。`cargo fuzz` の `hpack_decode`
   互換。tools/perf の HTTP/2 構成でベースライン比較。

詳細設計は [docs/artifacts/hpack_huffman_lut_design.md](../../artifacts/hpack_huffman_lut_design.md)
（設計フェーズで作成）。

## 期待効果

- HEADERS フレーム処理の CPU コスト削減（理論値: 単体 15〜20% 級の改善余地）
- L1I/L1D ミスと分岐予測ミスの削減
- HTTP/2 全体スループットの向上（tools/perf で確認）

## 受け入れ条件

- [ ] 線形探索デコードを LUT 方式へ置換（公開 API `huffman_decode` 互換）
- [ ] B-21 回帰・ラウンドトリップ・不正入力で panic なし
- [ ] `cargo test`（unit/integration）と E2E 通過
- [ ] features: full / default / no-default / 各主要 feature でビルド警告 0
- [ ] `clippy -D warnings` / `cargo fmt` クリーン、`allow(dead_code)` 不使用（やむを得ない場合は理由明記）
- [ ] tools/perf HTTP/2 を実行し、過去結果（`docs/perf/`）比で改善または非劣化を確認・記録
- [ ] backlog / README（en/ja）/ 必要なら AGENTS.md・config.toml を更新

## 依存・リスク

- **依存**: なし（`http2` feature 内の純アルゴリズム変更）
- **リスク**: LUT 生成バグによる誤デコード → 回帰テスト + fuzz で緩和
- **リスク**: テーブルサイズ過大による I-cache/D-cache 圧迫 → 4bit vs 8bit の設計比較で抑制
- **関連**: B-21（Huffman シフト panic）、F-26/F-116（HTTP/2 ホットパス最適化群）
