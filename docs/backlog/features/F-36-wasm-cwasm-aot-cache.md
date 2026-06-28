# F-36: WASM cwasm AOT 事前コンパイルキャッシュ

## 出典

`docs/artifacts/architecture_analysis_v4.md` / `v5.md`（cwasm による起動時オーバーヘッド排除）。

## 概要

起動時に `.wasm` を Wasmtime の Cranelift で JIT コンパイルするオーバーヘッドを、
AOT 済みバイナリ（`.cwasm`）のキャッシュ＋`deserialize` ロードで排除し、起動速度と
メモリフットプリントを改善する。

## 改修内容

1. `src/wasm/engine.rs` で `Engine::precompile_module()` により `.cwasm` を生成し、
   モジュールパス + ハッシュ + Wasmtime バージョンをキーにディスクキャッシュ（例: `/var/cache/veil/wasm/`）。
2. 次回起動時はキャッシュが有効なら `unsafe Module::deserialize_file()` でロード（JIT スキップ）。
   キャッシュ無効（ファイル変更・バージョン不一致）なら再コンパイルして更新。
3. seccomp の `PROT_EXEC` ブロックとの順序（既存の「JIT 完了後に制限適用」）を維持。
   deserialize でも実行可能メモリ確保が走るため、セキュリティ適用前に完了させる。

## 受け入れ条件

- [ ] `wasm` feature の E2E が回帰なし。
- [ ] 2 回目以降の起動で WASM コンパイル時間が短縮される。
- [ ] キャッシュ改ざん耐性（`deserialize` 対象の検証/権限）を考慮。

## 依存・リスク

- `Module::deserialize` は信頼境界に注意（`unsafe`、改ざんされた .cwasm の読み込み回避）。
- キャッシュディレクトリの権限と Landlock 書込許可パスの整合。

## 対応状況: 完了

`src/wasm/registry.rs` の `load_module` を拡張し、自動 AOT サイドカーキャッシュを実装。

- `.wasm` ロード時に `<path>.cwasm` を確認し、`.wasm` 以降に生成されていれば
  `Module::deserialize_file` で高速ロード（Cranelift JIT を回避）。
- 不在・古い・wasmtime 版不一致（deserialize 失敗）時は `from_file` でコンパイルし、
  `module.serialize()` の結果を `tmp → rename` でサイドカーへ書き出す（ベストエフォート。
  書込権限が無ければスキップしてコンパイル済みモジュールをそのまま使用）。
- 既存の「明示的 `.cwasm` 指定」経路はそのまま維持。
- モジュールロードは起動時（Landlock/seccomp 適用前）のため書込権限問題は通常発生せず、
  `deserialize` 失敗は常に再コンパイルへ安全フォールバック。信頼境界は元の `.wasm` と同じ
  （設定/キャッシュディレクトリのファイル完全性）。
- `.cwasm` は生成物のため `.gitignore` に `*.cwasm` を追加。

### 検証

- `cargo test --bins --features full wasm::` 100 通過（モジュールロード smoke 含む）。
- ロード後に `header_filter.wasm.cwasm`（約 742KB）が生成されることを確認。
- E2E（features full）の WASM 経路が回帰なし。
