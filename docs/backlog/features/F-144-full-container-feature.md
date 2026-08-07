# F-144: `full-container` feature（コンテナ向け全機能ビルド）

## 機能説明

デフォルトのデータプレーンランタイムは io_uring（`veil_rt_uring`）だが、コンテナ・
オーケストレーション環境（Docker、Kubernetes、gVisor 等）では seccomp プロファイルが
io_uring 関連の syscall（`io_uring_setup`/`io_uring_enter`/`io_uring_register` 等）を
許可しない、あるいは gVisor のようなユーザ空間カーネルエミュレーションが io_uring を
そもそも実装していない、といった理由で io_uring が利用不能なことが多い。既に
`--features epoll` で Linux 向けの epoll ベース readiness ランタイム（`veil_rt_reactor`）
へ明示的に切り替えられるが、コンテナ向けに `full`（全機能）+ `epoll` を毎回
`--features "full,epoll"` と手で組み合わせる必要があり、`full-freebsd`/`full-openbsd` 等
既存の複合 feature と一貫しない使い勝手だった。

## 改修内容

- `Cargo.toml` の `[features]` に `full-container` を追加。`full` と同じメンバー
  （`ktls`, `http2`, `http3`, `grpc-full`, `wasm`, `opentelemetry`, `compression`, `cache`,
  `metrics`, `websocket`, `rate-limit`, `buffering`, `mimalloc`, `admin`, `access-log`,
  `l4-proxy`）に `epoll` を加えたリスト（`full-freebsd`/`full-openbsd` 等と同じく
  `["full"] + [...]` の合成ではなくメンバーを列挙するスタイルに合わせた）。
  `default` は変更しない（AGENTS.md の default 不変則）。
  - `ビルド方法`: `cargo build --features full-container`（Linux 専用。`epoll` は
    `build.rs` が非 Linux ターゲットで指定されると panic するため、他 OS 向けの
    `full-freebsd`/`full-openbsd`/`full-netbsd` 等とは併用しない）。

## 受け入れ条件

- `cargo build --no-default-features --features full-container` が成功する。
- `cargo clippy --features full --all-targets -- -D warnings` 等、既存の検証コマンドに
  影響しない（`full-container` は既存 feature の組み合わせを追加しただけで、新規コードは
  無い）。
- README.md / docs/readme/README.ja.md の Cargo フィーチャー一覧に記載済み。

## 依存・リスク

- 新規コードなし（既存 `full` の feature セットと既存 `epoll` feature の組み合わせのみ）。
- `epoll` の Linux-only 制約（`build.rs` の panic）はそのまま引き継ぐため、非 Linux で
  `full-container` を指定すると（`full-freebsd` 等の代わりに誤って使うと）ビルド時に
  panic して即座に気づける。
