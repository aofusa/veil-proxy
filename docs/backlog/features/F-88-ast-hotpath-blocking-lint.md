# F-88: Rust AST 対応の静的解析（ホットパス ブロッキング呼び出し Lint）

出典: [container_security_review_report.md](../../artifacts/container_security_review_report.md) 提案5。親: [F-64](F-64-sast-semgrep.md)（SAST）。

## 目的

Semgrep の Rust 対応は正規表現（generic）ベースのため、AGENTS.md の「ホットパス絶対規則」で
禁じられている **ブロッキング呼び出し（`std::thread::sleep`・同期 I/O・`std::net` 等）** の
検出が誤検知回避のため意図的に除外されており、SAST で規約違反を自動検出できていない。
Rust の AST を理解する lint（clippy の `disallowed-methods` / `disallowed-types` 設定）を導入し、
プロダクションコードへの同期処理の混入をコンパイル（clippy）レベルでブロックする。

## 改修内容

- `clippy.toml` に `disallowed-methods` / `disallowed-types` を定義:
  - `std::thread::sleep`、`std::fs`（read/write/metadata/canonicalize 等の同期 FS）、
    `std::net::TcpStream::connect` / `std::net::TcpListener`（同期ソケット）、
    `std::io::copy` 等、ホットパスで禁止されるブロッキング API を列挙（理由メッセージ付き）。
- 正当な利用箇所（`src/runtime/offload.rs` のオフロード先ワーカー、起動時の一度きりの
  初期化、`#[cfg(test)]`・`tests/`・`benches/`・ツール類）には **理由を明記した個別
  `#[allow(clippy::disallowed_methods)]`** を付与する（bare allow は semgrep カスタムルールが
  監視する既存方針に合わせ、必ず理由コメントを添える）。
- `cargo clippy --all-targets --features full` が警告ゼロで通ることを確認し、
  container_security の semgrep フェーズ（または CI clippy）と役割分担を README に明記する。
- dylint（独自 lint クレート）は toolchain 負担が大きいため、まず clippy 設定で導線を作り、
  「`src/runtime/` 以外のモジュール別ポリシー」等の高度な条件が必要になった時点で再評価する。

## 受け入れ条件

- [x] `clippy.toml` の disallowed 設定が入り、違反を意図的に書くと clippy が検出すること。
- [x] 既存コードの正当な利用箇所すべてに理由付き allow が付与され、警告ゼロであること（B-26 含む）。
- [x] CI（`.github/workflows/ci.yml` の clippy ジョブ）でこの lint が有効なこと。

## 依存・リスク

- clippy の disallowed-methods はクレート単位のため、「ホットパスのみ禁止」の粒度は
  allow の運用（理由必須）で担保する。モジュール別ポリシーが必要なら dylint を再評価。
