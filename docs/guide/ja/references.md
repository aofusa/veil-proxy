# 参考資料・ロゴ

[← ドキュメント目次](README.md) · [English](../references.md)

## 参考資料

### コアライブラリ

- [monoio](https://github.com/bytedance/monoio): io_uringベースの非同期ランタイム
- [rustls](https://github.com/rustls/rustls): Pure Rust TLS実装
- [kTLS（独自実装）](https://docs.kernel.org/networking/tls.html): `src/ktls.rs` および `src/ktls_rustls.rs` として実装したカーネルTLSモジュール
- [httparse](https://crates.io/crates/httparse): 高速HTTPパーサー
- [quiche](https://github.com/cloudflare/quiche): Cloudflare製 QUIC/HTTP/3実装

### パフォーマンス

- [mimalloc](https://github.com/microsoft/mimalloc): 高速汎用メモリアロケータ
- [matchit](https://crates.io/crates/matchit): 高速Radix Treeルーター
- [ftlog](https://crates.io/crates/ftlog): 高性能非同期ログライブラリ
- [memchr](https://crates.io/crates/memchr): SIMD最適化文字列検索
- [Linux Huge Pages](https://docs.kernel.org/admin-guide/mm/hugetlbpage.html): Large OS Pages設定ガイド

### モニタリング

- [prometheus](https://crates.io/crates/prometheus): Prometheusメトリクスライブラリ

### CLI・並行制御

- [clap](https://crates.io/crates/clap): コマンドライン引数パーサー
- [arc-swap](https://crates.io/crates/arc-swap): ロックフリーなArc交換（設定ホットリロード用）
- [ctrlc](https://crates.io/crates/ctrlc): シグナルハンドリング（Graceful Shutdown用）
- [signal-hook](https://crates.io/crates/signal-hook): SIGHUPハンドリング（Graceful Reload用）
- [core_affinity](https://crates.io/crates/core_affinity): CPUアフィニティ設定

### カーネル機能

- [Linux Kernel TLS](https://docs.kernel.org/networking/tls.html): kTLSドキュメント
- [io_uring](https://kernel.dk/io_uring.pdf): io_uring設計ドキュメント
- [SO_REUSEPORT](https://lwn.net/Articles/542629/): ポート共有とロードバランシング

### セキュリティ

- [systemd.exec](https://www.freedesktop.org/software/systemd/man/systemd.exec.html): systemdセキュリティ設定
- [seccomp](https://docs.kernel.org/userspace-api/seccomp_filter.html): Seccomp BPFフィルタ
- [Landlock](https://docs.kernel.org/userspace-api/landlock.html): ファイルシステムサンドボックス
- [io_uring Security](https://www.kernel.org/doc/html/latest/userspace-api/io_uring.html): io_uringセキュリティ考慮事項
- [bubblewrap](https://github.com/containers/bubblewrap): 非特権サンドボックスツール

### WASM拡張

- [Proxy-Wasm](https://github.com/proxy-wasm/spec): Proxy-Wasm ABI仕様
- [Wasmtime](https://wasmtime.dev/): WebAssemblyランタイム
- [proxy-wasm-rust-sdk](https://github.com/proxy-wasm/proxy-wasm-rust-sdk): Rust SDK

## ロゴ

<table align="center">
  <tr>
    <th align="center">メインロゴ (WebP)</th>
    <th align="center">代替ロゴ (SVG)</th>
    <th align="center">ロゴ文字 (SVG)</th>
  </tr>
  <tr>
    <td align="center">
      <img src="../../images/veil_logo.webp" alt="Veil メインロゴ" width="200" />
    </td>
    <td align="center">
      <img src="../../images/veil_logo_alternative.svg" alt="Veil 代替ロゴ" width="200" />
    </td>
    <td align="center">
      <img src="../../images/veil_logo_text.svg" alt="Veil ロゴ文字" width="200" />
    </td>
  </tr>
</table>
