# F-276: v0.10.0 の全体検証（全プラットフォームの試験・警告・性能・セキュリティ）と、rc での CI の確認・公開

**状態: 未着手（2026-10-25 起票・計画。親は [F-258](F-258-v0100-plan.md)。F-259〜F-275・B-164〜B-166 を main に入れた後に着手）**

## 依頼（2026-10-25）

F-258 の子チケットの実装が全部終わった後に、次の 1〜3 を確認し、問題があれば直す。問題が無くなるまで 1〜3 を繰り返す。
その後 `v0.10.0-rc.N` で CI（`release.yml`）の動作を確かめ、問題が無ければ `v0.10.0` のタグを打ち、リリースノートを作って公開する。
**タグの作成・push・GitHub Release の公開・crates.io・PyPI・イメージ等の公開は、利用者から許可を得ている（2026-10-25）**。
ベンチマークの提出（[F-269](F-269-benchmark-submissions.md)）は公開の後で、各公開行為の前に確認を取る（こちらは許可の対象外）。

## 1. 全プラットフォームの試験と警告

### 1.1 警告（Linux x86_64 のこの端末）

- ビルドの組み合わせ: `--features full`・既定の features・`--no-default-features`・**`[features]` の各 feature を 1 個ずつ**
  （`--no-default-features --features <f>`）・`full-container`・ライブラリ（`api`・`api-core`・`handler` 単体・`grpc-handler`・
  `tokio-compat-bridge` 等）・ワークスペースの各クレート（`veil-proxy-kit` の各 feature を含む）。各 OS の feature セット
  （`full-freebsd`・`full-openbsd`・`full-netbsd`・`full-macos`・`full-windows`）はその OS の上（下の 1.2）かクロスビルド
  （`packaging/scripts/build-cross.sh`）で。
- 全部で `warning` を 0 にする。`cargo clippy`（同じ組み合わせ、`--all-targets`）・`cargo fmt --check`・
  `tools/lint/check-messages.py`・`tools/release/check-docs.sh` も 0 にする。
- **`allow(dead_code)` は使わない**。どうしても要る場合だけ、理由をすぐ上のコメントに書き、妥当かを判断して記録する
  （AGENTS.md は禁止。原則はプラットフォーム限定の `cfg` を付けて解決する）。`#[allow(clippy::…)]` も論拠の無いものは足さない。
- 組み合わせの一覧と結果は `docs/artifacts/v0100/warnings.md` に残す（各 feature のビルドを回す台本は `tools/lint/` に置くかを実施時に決める）。

### 1.2 試験の行列

| OS | アーキ | 実行する場所 | 方法 |
|---|---|---|---|
| Linux | x86_64 | この端末 | `cargo test --lib --bins --test integration_tests --features full`、`./tests/e2e_setup.sh test`（io_uring）、`VEIL_E2E_FEATURES="full,epoll" ./tests/e2e_setup.sh test`（reactor）、`tests/api/library_e2e.sh all`（+ `external`）、`crates/veil-python/tests`、`cargo test -p veil-capi`、`veil-proxy-kit`・`veil-api-kit` の試験、`tests/` の各台本（`tests/README.md` の一覧） |
| Linux | aarch64 | `ssh llm` の QEMU | `tools/qemu/linux-aarch64-e2e.sh`・`run-e2e-aarch64.sh`（TCG で詰まる既知の問題があるので、HVF で動く経路を使う。だめなら aarch64-musl のクロスビルド + 単体の範囲と記録） |
| FreeBSD・NetBSD・OpenBSD | x86_64 | この端末の QEMU | `tools/qemu/bsd-vm.sh` で VM を起動し、`cargo test`・E2E・`bsd-security-e2e.sh`（capsicum・pledge/unveil・chroot）・`fbsd-capmode-e2e.sh` |
| FreeBSD・NetBSD・OpenBSD | aarch64 | `ssh llm` の QEMU（HVF） | 同上（`fbsd-arm64-vm.sh`・`aarch64-vm.sh`） |
| Windows | x86_64 | `ssh aofish`（`C:\Users\5000e\Documents\aofusa\projects\veil\src\veil-proxy`） | `cargo test`（`full-windows`）・E2E の Windows 版・API ライブラリ・Python の wheel・C の例（可能な範囲） |
| macOS | aarch64 | `ssh llm`（`/Users/nttsmc/Documents/bi/other/projects/veil/src/veil-proxy`） | `cargo test`（`full-macos`）・E2E・API ライブラリ・Python・Seatbelt（可能な範囲） |

- 各端末の作業ディレクトリへ、今のソースと実行に要るもの（`target/` を除く。`tests/fixtures`・証明書の生成台本・`third_party/`）を
  コピーしてから実行する（`rsync`・`scp`。Windows は bundle か zip）。
- 失敗した項目は原因を調べて直す（直したら該当する OS で回し直す）。不具合はバグのチケットに登録してから直す。
- 結果は `docs/perf/platform_verification.md` を v0.10.0 の内容に置き換える（過去の記載は消す）。

### 1.3 運用上の注意（過去の記録から）

- 長い試験は `nohup` + `disown` + フラグのファイルで走らせ、待つのは Monitor（Bash の background の sleep のループは殺される）。
- E2E の前の掃除は `pkill -x veil`（`-f` は自分のシェルを巻き込む）。`/home/aofusa`・`/`・`/tmp` の空き（docker の builder cache と
  `target/debug/incremental`）を先に確かめる。`ssh llm` の macOS は QEMU を重ねて動かすとネットワークごと落ちる（同時に 1 台）。
  BSD の VM の起動中に `bsd-vm up` しない（UFS が壊れる）。実行中の台本を編集しない。

## 2. 性能（tools/perf）

### 2.1 計測

| 対象 | 場所 | 方法 |
|---|---|---|
| Linux x86_64 | この端末の Docker | `tools/perf/suite_ab.sh`（v0.9.1 と v0.10.0 を交互、全構成 × 3 反復）、`vs_nginx.py`、ライブラリは `tools/api-bench`（axum・FastAPI と交互）。HTTP/1.1・HTTP/2・h2c・HTTP/3・gRPC・WebSocket・静的配信・中継・圧縮、3B と 54KB、io_uring と epoll |
| FreeBSD aarch64 | `ssh llm` の VM | `tools/perf/bsd/`（`run_perf_bsd.sh`・`cpu_per_req.sh`。VM は `*_cpu_us_per_req` で比べる） |

- 計測の原則（AGENTS.md）: 交互 A/B、同じ条件（nginx にも `open_file_cache`、kTLS は両方オフ）、定常状態、errors 列を先に見る。
  ベースラインは `git worktree` で v0.9.1 を取り出してビルドする。

### 2.2 退行と nginx

- **退行**（v0.9.1 比で中央値が 0.95 未満、または errors が増えた構成）は、原因を調べて直す（測り直しで再現を確かめてから）。
- **nginx に劣る構成** は、原因を調べ、仮説を立て（strace・perf（特権コンテナ経由）・allocs の計測）、性能を上げる改修をする。
  大きな変更も許容されている。最高の性能を目指す。改修のたびに単体・統合・E2E を通す。
- 退行が無くなり、nginx を超えるまで「計測 → 調査 → 改修」を繰り返す。改修はチケット（機能）に登録してから行う。
- 最終の結果を `tools/perf/README.md`（結果の節）と `docs/perf/` に書く。**`docs/perf/` は最新の結果だけ** にし、過去の生データ・記載
  （`linux_v084_ab_raw.tsv`・`linux_v090_ab_raw.tsv` 等）は消す（過去は git の履歴で辿れる）。

## 3. セキュリティとカオス

- `tools/container_security/run.sh`（ファジング・カオス・h2spec・h3spec・testssl・sanitizer・grype・Trivy・cargo-audit・cargo-deny）を回す。
  判定は「本当に走ったか」を見る（打ち切り・常時スキップ・起動失敗を ok にしない。AGENTS.md）。新しい攻撃面（WebSocket の復号器・
  multipart の解析器・Wasm の本文フェーズ・ファイルの変更通知・`[[listener]]`）のファジングの対象が入っていることを確かめる。
- Claude の security プラグイン（`claude-security`）でリポジトリ全体の検査をする。
- 見つかった問題は **全部チケットに登録** して直す。直したら 2 の計測もやり直し、問題が無くなるまで 1・2・3 を繰り返す。

## 4. rc と公開

1. 版を上げる: `tools/release/bump-version.sh 0.10.0-rc.1`（ワークスペース・`crates/*`・`pyproject.toml`・packaging）。
   リリースノート `docs/releases/v0.10.0.md` を書く（`release.yml` が存在を確かめる。既定の設定の変化（新規インストールの `[tls_client] ca = "system"`）・
   互換の注意（`tls_ca` に `system`・`builtin` という名前のファイル、Python の `async def` の既定の実行先、`[[listener]]` の `secure_redirect` の既定）・
   新しいパッケージ（`veil-proxy-kit`・`veil-api-kit`）・性能の要約）。
2. `v0.10.0-rc.1` のタグを push し、`release.yml`（プレリリースは crates の dry-run・TestPyPI・版のタグだけのイメージ）と `dist.yml`・
   `release-downstream.yml` の確認できる部分が全部緑になることを確かめる。失敗したら直して `rc.2` …。
   - **新しいパッケージの最初の公開の準備**: crates.io の trusted publishing は既存のクレートにしか設定できず、PyPI も新しい
     プロジェクトは「pending publisher」の登録が要る（`packaging/README.md` の「公開（リリース）」の節）。`veil-proxy-kit`・`veil-api-kit` の
     最初の公開に人の作業（トークンでの初回公開か、pending publisher の登録）が要るかを rc の前に確かめ、要るなら利用者に依頼する。
3. 問題が無ければ版を `0.10.0` にして `v0.10.0` のタグを push → `release.yml` が draft を作る → 内容（成果物・チェックサム・署名・
   本文）を確かめて **公開する**（`gh release edit v0.10.0 --draft=false`。利用者の許可済み）→ `release-downstream.yml`（Docker Hub・
   Homebrew・apt/dnf・winget 等）が緑になり、各配布先から入れて動くことを確かめる（`tools/release/*-test.sh`）。
4. 公開物（crates.io・PyPI・ghcr・GitHub Release）がそろったら F-269 に進む。

## 受け入れ条件

- 1〜3 がすべて問題無し（記録が `docs/perf/`・`docs/artifacts/v0100/` にある）。v0.10.0 が公開され、下流の配布先が更新されている。
- 見つけた問題がチケットになって完了している。`backlog.md` が更新されている。
