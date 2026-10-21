# F-244: ライブラリとして使うとき、既定でアクセスログを標準出力に出す

**状態: 完了（feat/v083-release、2026-10-21。v0.8.3 で公開）**

## 目的

API ライブラリ（Rust の `handler` feature・Python の `veil-api`・C の `veil-capi`）として起動したとき、設定を書かなくても
アクセスログが標準出力に出るようにする。FastAPI（uvicorn）や axum の例と同じく、開発中にすぐ要求を見られるようにするため。

プロキシのバイナリ（`veil`）の既定は変えない（`[access_log] enabled` は既定 `false` のまま）。

## 今の状態（2026-10-21 の main = v0.8.2）

- ライブラリの既定の設定は `src/api/default_config.toml` である。
  - 合成の順は「既定 < ファイル < VDS < 環境変数 < インライン < ビルダ」（`config_override`）。
  - `[access_log]` の節は無い。
- アクセスログの出方は、`access-log` feature の有無で変わる（`src/logging.rs` の `access_outputs_active`・`log_access`）。

  | feature | 出力 | ライブラリの既定（`[logging] level = "info"`）での結果 |
  |---|---|---|
  | `access-log` あり | `access_log::log_access_structured`（JSON / text、専用スレッド、既定は stdout） | **出ない**（`AccessLogConfig::enabled` の既定が `false`） |
  | `access-log` 無し | ftlog の `info!(target: "access", "Access: ...")` | 出る（`info` で有効なので） |

- 案内している依存の書き方（`features = ["full", "api"]`）は `full` に `access-log` を含む。Python と C のクレートも
  `access-log` を有効にしている（`crates/veil-python/Cargo.toml`・`crates/veil-capi/Cargo.toml`）。
  つまり、普通に使うとアクセスログは出ない。feature を絞ったときだけ出る、という分かりにくい状態である。
- ハンドラの経路（HTTP/1.1・HTTP/2・HTTP/3 の `Backend::Handler`）も、プロキシと同じ共通の `log_access` を通る
  （例: `src/proxy/h1.rs` の dispatch の後）。そのため、ハンドラのためにログを書き足す必要は無い。

## 設計

- `src/api/default_config.toml` に次を足す。

  ```toml
  [access_log]
  # ライブラリの既定はアクセスログを標準出力に出す（プロキシのバイナリの既定は false）。
  # 止めるときは config.toml で enabled = false、または Config::builder().set("access_log.enabled=false")。
  enabled = true
  format = "text"
  ```

  - 形式は `text` にする。開発中に端末で読むため。本番で集める用途は `json` を選んでもらう（文書に書く）。
  - 出力先は既定（`file_path` を書かない = stdout）のまま。
  - `flush_interval_ms` は既定の 1000 のままにする。端末に出るのが最大 1 秒遅れるので、文書に書く。
    短くすると書き込みの syscall が増えるため、既定は変えない。
- `access-log` feature が無いビルドは、今と同じ ftlog のテキストの経路で出る。結果として、どの feature の組み合わせでも
  既定で出ることになる。
- `[logging]` の既定（stdout）と同じストリームに混ざる。区別はログの行の `type=access` でつく（今の仕様のまま）。
- 止める方法は今ある手段で足りる。ビルダに専用のメソッドは足さない。
  - `config.toml` の `[access_log] enabled = false`
  - `VEIL_OVERRIDE_<n>="access_log.enabled=false"`
  - `Config::builder().set("access_log.enabled=false")`
  - Python・C も同じ上書きの経路で止められる。

### 性能

- アクセスログが有効になると、要求ごとに次の処理が増える。
  - ログの行の組み立て（スレッドローカルのバッファ）と、専用スレッドへの送信
  - 時刻の読み取り（`request_start_instant` が `Instant::now()` になる）
- これは利用者が選べる既定の値の変更であり、ホットパスのコードは変えない。
- **計測の条件を揃える。** API のベンチ（`tools/api-bench`、F-201・F-202）は、比較相手（axum・FastAPI）がアクセスログを出さない
  条件で測っている。veil 側の設定で `enabled = false` を明示し、0.8.2 と同じ条件にする。
  - 有効にしたときのコストは参考の値として測り、チケットに記録する。対象はハンドラの 3B の応答の rps と
    `cpu_us_per_req`（交互 A/B）。
- `tests/api/library_e2e.sh nonregression` が見る「プロキシのバイナリに入っていないもの」は変わらない（既定の設定の値だけの変更）。

### 文書

- `docs/guide/library.md`（英日）に「Access log」の小節を足す。内容は次のとおり。
  - 既定で stdout に text で出る。
  - 止め方と、JSON・ファイルへの切り替え。
  - 1 秒のフラッシュの間隔。
- `crates/veil-python/README.md` と `crates/veil-capi/README.md` の設定の節に 1 行ずつ足す。
- `examples/config.toml` の `[access_log]` のコメントに、「ライブラリの既定は enabled = true」と 1 行足す（AGENTS.md
  「外部契約を変えたら examples/config.toml も」）。

## 試験

- 単体: `src/api/config.rs` の既定の設定の試験に、`access_log.enabled == true`・`format == "text"` の確認を足す。
  `config.toml` とビルダで `false` に上書きできることも確かめる。
- `tests/api/library_e2e.sh` に場合を足す。
  - Rust・Python・C の各例で、要求を 1 回送り、標準出力に `type=access` の行（メソッド・パス・状態）が出る。
  - `access_log.enabled=false` の上書きで、その行が出なくなる。
- プロキシのバイナリの E2E（`./tests/e2e_setup.sh test`）は、既定が変わらないので今のまま通る。

## 受け入れ条件

- 設定を何も書かない Rust・Python・C のライブラリの例で、要求ごとにアクセスログが stdout に出る。
- 上の 3 つの方法のどれでも止められる。
- プロキシのバイナリは、設定を書かなければ今と同じでアクセスログを出さない。
- API のベンチで、`enabled = false` の条件の値が 0.8.2 と同等（交互 A/B で ±2% 以内）。

## 実装の結果

- 既定の設定に `[access_log] enabled = true`・`format = "text"`。単体試験 1 件。
- `library_e2e.sh rust` で、既定で `type=access ... path=/hello` が標準出力に出ること、`VEIL_OVERRIDE` で止まることを確かめた
  （C・Python は e2e に確認を足し、全プラットフォームの試験で回す）。
- API ベンチは `VEIL_OVERRIDE_90="access_log.enabled=false"` を既定にした。

## 関連

- [F-188](F-188-api-library-core.md)（ライブラリの既定の設定）
- [F-201](F-201-api-library-benchmark.md)・[F-202](F-202-api-handler-path-perf.md)（API のベンチ）
- [F-245](F-245-version-0-8-3.md)
