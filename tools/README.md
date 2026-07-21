# tools — 運用・検証ツール群

`veil-proxy` の **本体ソース（`src/`）や単体・統合・E2E テスト（`tests/`）とは独立した、Docker ベースの検証・計測ツール** を集約するディレクトリです。いずれもホストへの追加インストールを最小化し、**docker コマンドのみ** で完結することを原則とします。

| ディレクトリ | 役割 | 概要 |
|--------------|------|------|
| [`perf/`](perf/) | パフォーマンス計測 | `veil:glibc` / `veil:musl` と `nginx:alpine` をコンテナ間通信で比較し、HTTP/1.1・HTTP/2 のスループット / レイテンシ / CPU / メモリを TSV に集約するハーネス |
| [`container_security/`](container_security/) | セキュリティ検証 | Veil の Docker イメージを対象にファジング・カオスエンジニアリング・HTTP/2 準拠（h2spec）・セキュリティスキャン（Trivy / cargo-audit / cargo-deny / testssl）を実行するオーケストレータ |
| [`qemu/`](qemu/) | プラットフォーム×arch 検証 | full-system QEMU で **他アーキ/他 OS の実カーネル**上でのビルド・E2E・perf を検証（Docker ヘルパ経由）。Linux aarch64 io_uring（`aarch64-vm.sh` + `run-e2e-aarch64.sh`）、**FreeBSD arm64 のネイティブビルド + 動作確認**（`fbsd-arm64-vm.sh`。TCG でも実用起動）。詳細は [`qemu/README.md`](qemu/README.md) |

---

## なぜ `tools/` か

- **perf**（旧 `docker/perf/`）と **container_security**（旧 `tests/container_security/`）は、どちらも「Docker イメージを起動して外側から負荷・攻撃・計測を当てる」ブラックボックス検証で、性質が共通しています。
- `tests/` は `cargo test` が拾うホワイトボックス（単体・統合・E2E）に絞り、外形検証ツールは `tools/` に分離することで役割が明確になります。
- 計測・検証に使う静的アセット（TLS 証明書・配信コンテンツ・seccomp 許可リスト）は引き続き [`docker/assets/`](../docker/assets/) を共有参照します（複製しません）。

---

## クイックスタート

```sh
# パフォーマンス計測（前提: veil:glibc / veil:musl イメージをビルド済み）
bash tools/perf/gen_configs.sh      # 計測用 config バリアント生成
bash tools/perf/run_perf.sh         # nginx と比較計測

# コンテナセキュリティスイート（前提: veil:glibc イメージをビルド済み）
./tools/container_security/run.sh
```

各ツールの詳細な前提条件・環境変数・出力フォーマットは、それぞれの README を参照してください。

- [`perf/README.md`](perf/README.md)
- [`container_security/README.md`](container_security/README.md)

イメージのビルド手順は [`docker/README.md`](../docker/README.md) を参照してください。
