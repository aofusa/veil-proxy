# F-195: ライブラリ成果物の packaging・Docker ビルド・examples・ドキュメント

**状態: 進行中（feat/f188-api-library-vds-k8s）**

## 目的

ビルド時に「Rust ライブラリ / C ライブラリ / Python モジュール」を選んで作れるようにし、packaging で
ライブラリ成果物を **1 個の zip** にまとめる（GitHub Release で既存の veil バイナリと混ざらないように）。

## 設計

- `packaging/scripts/build-lib.sh --kind all|rust|c|python`、`docker/Dockerfile.lib`（manylinux_2_28 ベース）で Docker ビルド。
- 出力: `packaging/output/lib/veil-lib-<version>-<arch>-linux-gnu.zip`（c/・python/・rust/・docs/）。
- examples: `examples/api/`（Rust・C・Python）。ドキュメント: `docs/guide/library.md`（英日）。

## 受け入れ条件

- Docker でのビルドが通り zip の中身（.so/.a/.h/.whl/ソース）が揃う。zip から取り出した成果物でサンプルが動く。
