# フェーズ 2: aarch64（ARM64）対応

## 目的

AWS Graviton、Apple Silicon サーバ、エッジ ARM デバイス等で **同等にビルド・実行**できるようにする。

## スコープ案

1. **ビルド**
   - `cargo build --target aarch64-unknown-linux-gnu`（および musl）の CI マトリクス。
   - 依存クレートの ARM 対応確認（aws-lc-rs、io_uring、asm 最適化の有無）。

2. **パフォーマンス**
   - kTLS、CBPF、CPU アフィニティの挙動差をベンチで記録。

3. **バグ修正**
   - アライメント、`#[cfg(target_arch)]` 分岐、endian（主にネットワークはビッグエンディアンだがコード内の仮定確認）。

## 受け入れ条件（案）

- aarch64 で既存テストの大部分が緑（kTLS 等ハード依存は条件付きスキップで可）。
- リリースアーティファクトに aarch64 バイナリまたはビルド手順。

## 前提

- Linux aarch64 を第一ターゲット。macOS サーバ利用は二次。
