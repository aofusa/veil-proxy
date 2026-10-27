# B-243: semgrep のカスタムルール `veil-no-bare-allow-dead-code` が、コメント内の文字列と `examples/`・`third_party/` に一致して毎晩 37 件の偽の所見を出す

**状態: 修正済み（v0.10.0）**

## 出典

2026-10-27 に aofish の Docker Desktop で回した `tools/container_security/run.sh`（v0.9.2 の `veil:glibc`）の `semgrep_report.txt`・`semgrep.sarif`
（`docs/artifacts/security/scans/aofish-glibc/`、git 管理外）。

## 事象

`.semgrep/veil-rules.yml` の `veil-no-bare-allow-dead-code` は `languages: [generic]` + `pattern-regex: '#\[allow\(dead_code\)\]'` で、
`paths.include: ["src/"]`。2299 件の所見のうちこのルールが 37 件で、**全部が偽**:

- `src/cache/file_cache.rs:99,494`・`src/cache/resolve.rs:45`・`src/cache/static_file.rs:205,250,274`: rustdoc・コメントの中の
  「`#[allow(dead_code)]` は規約で禁止」という**文章**に一致している（generic モードはコメントを区別しない）。
- `examples/wasm-filters/*`（13 件）・`third_party/quiche`・`third_party/wasmtime`（18 件）: `paths.include: "src/"` が効いていない。
  スキャンの root が `/src`（`run_semgrep.sh` の `-v "${REPO_ROOT}:/src:ro"`）なので、全ファイルの path に `src/` が含まれて
  include に一致してしまう。

本物の `#[allow(dead_code)]` は `src/` に 0 件だった（規約は守られている）。偽の所見が毎晩 37 件出るので、本物が混ざっても気づけない。
semgrep は非ブロッキング（`--error` 無し）なので suite は失敗しないが、B-217 の集計では passed に見える。

## 改修案

- `pattern-regex` を行頭の属性だけに（`^\s*#\[allow\(dead_code\)\]`）し、`languages: [rust]` の `pattern: #[allow(dead_code)]` に替えられるなら
  コメントを除外できる（semgrep の Rust の対応状況を見る）。
- `paths.include` を `"^src/"`（絶対の `/src/src/`）か、`run_semgrep.sh` のマウント先を `/repo` に変えて `src/` との取り違えを無くす。
  `exclude` に `third_party/`・`examples/` を足す。
- 同じ `run_semgrep.sh` の `temp-dir`・`args` の所見（11 + 7）はすべて `#[cfg(test)]` の中か計測ツールなので、`p/security-audit` の該当ルールを
  `tests`・`tools/` で除外する（`.semgrepignore`）。

## 受け入れ条件

- `run_semgrep.sh` の `veil-no-bare-allow-dead-code` の所見が 0 件。`src/` に本物の `#[allow(dead_code)]` を置くと 1 件になる。

## 修正（v0.10.0）

- `.semgrep/veil-rules.yml`: `veil-no-bare-allow-dead-code` の regex を行頭の属性だけ（`(?m)^[ \t]*#\[allow\(dead_code\)\]`）にした（rustdoc・コメントの中の説明に一致しない）。2 つのルールの `paths.include` を走査の root からの `/src/` にし、`/third_party/`・`/examples/` を除外した。
- `run_semgrep.sh`: リポジトリを `/repo` に置き、作業ディレクトリから `.` を走査する（`/src` に置くと全ファイルのパスに `src/` が入り include が全部に一致していた）。カスタムルールの指定も `/repo/.semgrep`。
- 受け入れ: semgrep 1.182.1（venv）でカスタムルールだけを走らせ、今のリポジトリで 0 件、`src/lib.rs` に `#[allow(dead_code)]` を置くと 1 件（`veil-no-bare-allow-dead-code src/lib.rs`）。
- `p/security-audit` の `temp-dir`・`args` の所見（試験・計測ツール）は残した（このチケットの受け入れの外。偽の所見として F-280 で扱う）。

## 関連

- [F-64](../features/F-64-sast-semgrep.md)、B-217（集計の偽 OK。feat/v0100 で修正済み）
