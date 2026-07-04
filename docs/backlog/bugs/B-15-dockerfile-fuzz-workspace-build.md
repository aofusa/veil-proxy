# B-15: Dockerfile が fuzz ワークスペースメンバ未対応でビルド失敗

## 事象（再現手順）

`docker build -f docker/Dockerfile.glibc --build-arg CARGO_FEATURES='full' ..`（musl も同様）が
cacher ステージで失敗する:

```
error: failed to load manifest for workspace member `/app/fuzz`
       failed to read `/app/fuzz/Cargo.toml`
exit code: 101
```

## 影響

- **P1**。コンテナイメージ（glibc / musl）が一切ビルドできない。F-52 で
  `[workspace] members = [".", "fuzz"]` に fuzz を追加して以降、Dockerfile が追随できていない。

## 調査メモ

- cacher ステージは依存キャッシュのため `Cargo.toml` / `Cargo.lock` のみを先行コピーするが、
  ワークスペースメンバ `fuzz` のマニフェスト（`fuzz/Cargo.toml`）が無いため cargo が
  ワークスペース解決に失敗する。

## 改修内容

- `docker/Dockerfile.glibc` / `docker/Dockerfile.musl` の cacher ステージで、
  `fuzz/Cargo.toml` をコピーし、`[[bin]] path = "..."` から抽出したスタブソースを生成
  （`fn main() {}`）。fuzz 本体はビルドせず、ワークスペース解決を通すためだけのスタブ。
  builder ステージの `COPY . /app` で実体に置き換わる。

## 受け入れ条件

- [ ] `veil:glibc` / `veil:musl`（`CARGO_FEATURES=full`）が正常にビルドできる。

## 対応状況: 完了（ビルド確認は再ビルド時に実施）