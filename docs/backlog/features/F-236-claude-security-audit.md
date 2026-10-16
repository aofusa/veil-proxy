# F-236: Claude Security のプラグインでセキュリティ監査を行う

**状態: 未着手（2026-10-16 起票・設計）**

## 目的

Anthropic 公式のプラグイン claude-security（https://github.com/anthropics/claude-plugins-official/tree/main/plugins/claude-security）で、
リポジトリ全体のセキュリティ監査を行う。見つかった脆弱性を直してから、v0.8.0 を公開する（F-237・F-238 でイメージと
パッケージを広く配る前に行う）。

## プラグインの要点（2026-10-16 に README で確かめた）

- **導入:** Claude Code のセッションで `/plugin install claude-security@claude-plugins-official` を実行し、`/reload-plugins` で
  読み込む。マーケットプレイスが無ければ、先に `/plugin marketplace add anthropics/claude-plugins-official` を実行する。
- **前提:** Python 3.9 以上が PATH にあり、git のチェックアウトであること。
- **実行:** `/claude-security` で次の 3 つの仕事ができる。
  - リポジトリ全体か一部の走査（scope と effort を選ぶ。走査の前に費用の見積もりが出る）。
  - ブランチ・PR・コミットの差分の走査。
  - 指摘からのパッチの提案。パッチは作業ツリーの外で作って検証し、自動では当てない。
- **出力:** `CLAUDE-SECURITY-<時刻>/` に次のファイルが出る。
  - `RESULTS.md`（深刻度は CVSS v4.0 に合わせた CRITICAL〜LOW、確度、攻撃の筋書き、走査した範囲と除外の理由）
  - `RESULTS.jsonl`（`claudeSecurityPluginFindingId` は走査をまたいで同じ）
  - `RESULTS.sarif`
  - `REVISION-<sha12>.json`
  - パッチの提案を頼んだときは `patches/F<n>.patch`
- **その他:**
  - ディレクトリには `.gitignore` が入る。
  - 鍵などの値は伏せられるが、最善の努力でしかない。
  - 結果は走査ごとに変わり得る（非決定的）ので、繰り返すことで網羅が増える。
  - 推奨のモデルは Opus 5.5・Fable 5.1。

## 設計

### 走査の組み立て

全体の走査を 1 回だけ行うのではなく、攻撃面ごとに範囲を絞った走査を effort `high` で重ねる。全体の走査は網羅の確認に使う。

| # | 範囲 | 主な観点 |
|---|---|---|
| 1 | リポジトリ全体（effort `medium`） | 網羅の確認・鍵の混入（テストの鍵・フィクスチャを含む） |
| 2 | HTTP の解析: `src/proxy.rs`（HTTP/1.1）・`src/http2/`・`src/http3_*.rs`・`src/http_utils.rs`・`src/protocol/` | リクエストスマグリング・ヘッダの検証・HPACK と QPACK の爆弾・CONTINUATION と rapid reset・本文の長さ |
| 3 | TLS と認証: `src/upstream_tls.rs`・`src/x509_san.rs`・`src/jwt_verify.rs`・`src/simple_tls.rs`・`src/tls_*.rs`・`src/ktls*.rs`・`src/acme/` | SAN の照合・JWT のアルゴリズムの混同・`none`・JWKS の取得・証明書の差し替えの競合 |
| 4 | サンドボックスと特権: `src/security.rs`・`src/system.rs`・`src/connect_broker.rs`・seccomp の許可リスト・Landlock・capsicum・pledge | 許可リストの漏れ・ブローカーへの入力・特権降格の順序 |
| 5 | 静的配信とキャッシュ: `src/cache/`・静的配信の経路 | パスの走査（`openat2` の `RESOLVE_BENEATH` と従来経路への戻り）・シンボリックリンク・キャッシュの汚染 |
| 6 | 管理と制御: Admin API・`src/vds*.rs`・`src/ctrl_http.rs`・`src/config_override.rs`・`src/metrics.rs` | 認証の欠落・管理 API の露出・設定の注入 |
| 7 | 拡張: `src/wasm/`・`src/api/`・`crates/veil-capi`・`crates/veil-python` | Proxy-Wasm のホストの ABI の境界の検査・FFI の生ポインタ・Python の GIL と寿命 |
| 8 | メッシュ: `src/mesh/` | xDS・SDS の鍵の扱い・HBONE・ピアの ID の検証・認可（RBAC）の評価の漏れ |
| 9 | ランタイムとアロケータの `unsafe`: `src/runtime/`・`src/veil_alloc/`・`src/udp/` | io_uring のバッファの寿命（取り消し時）・CMSG の境界 |
| 10 | 供給網と配布: `.github/workflows/`・`docker/`・`packaging/`・`build.rs`・`third_party/` の差分（`README.veil.md` が挙げる差分だけ） | workflow の注入（`${{ }}` の展開）・権限・F-237・F-238 の鍵の扱い |

- 10 は F-237・F-238 の workflow を書いた後に、差分の走査（scan changes）で再度行う。
- `third_party/` は vendoring したコードなので、プラグインは既定で除外する。veil が変えた差分だけを対象にする。
- 既存の道具（`tools/container_security/` の cargo-audit・cargo-deny・semgrep・gitleaks・testssl・ZAP・スマグリング・
  h2spec と 11 個のファズ）とは役割を分ける。道具が既に見ている種類の指摘は、照合して重複を数える。

### 結果の扱い

- 出力は `docs/artifacts/claude-security/<時刻>/` に置く（git の管理外。AGENTS.md）。
- リポジトリは公開されているので、直す前の脆弱性の詳細を公開のチケットや commit に書かない。
  - 確認された指摘は、修正が main に入るまで、詳細を `docs/artifacts/` に置く。
  - B- チケットは、修正と同じ変更で起票する。題は「〜の検証の不足」程度に留める。
  - 修正の後に、チケットに詳細を追記する。
  - SARIF を GitHub の code scanning に上げるのも、修正の後にする。
- 深刻度と優先度の対応: CRITICAL・HIGH は P0（リリースを止める）、MEDIUM は P1、LOW は P2。
- 修正には、プラグインのパッチの提案を下書きとして使い、ホットパス規則・`cargo test`・E2E・`suite_ab.sh` を通してから入れる。
  プラグインはパッチを自動では当てない。
- 誤検出と判断した指摘は、理由を `docs/artifacts/claude-security/triage.md` に書く。`claudeSecurityPluginFindingId` で次の
  走査と照合する。
- 修正した後に、同じ範囲をもう一度走査し、指摘が消えたことを確かめる。

## 受け入れ条件

- 表の 1〜10 の走査を終え、`REVISION-*.json` の深刻度の件数と網羅（走査した範囲・除外の理由）を、このチケットに要約する。
  脆弱性の詳細は修正の後に書く。
- CRITICAL・HIGH の指摘が 0 になる（修正したか、誤検出と根拠を付けて判断した）。
- 修正の変更ごとに、回帰の試験（単体・E2E・ファズのコーパス）と、性能の非退行（`suite_ab.sh`）を確かめる。
- `docs/guide/security.md`（英日）に、監査の方法と再走査の手順を書く。

## 関連

F-237、F-238（公開の前に行う）、F-64〜F-72・F-84〜F-88（既存のセキュリティ試験）、`tools/container_security/`。
