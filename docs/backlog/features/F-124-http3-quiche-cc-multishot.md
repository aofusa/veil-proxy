# F-124: HTTP/3 quiche 輻輳制御・バッチ設定 + 低レベル API + io_uring RECVMSG multishot

## 機能説明

1. **`[http3]` で quiche `Config` の輻輳制御 / Pacing / HyStart を設定可能にする**
   - `cc_algorithm`（既定 `bbr`）、`pacing`（既定 true）、`max_pacing_rate`、`hystart`（既定 true）
2. **UDP バッチ幅を既定 64 にし、設定可能にする**
   - `mmsg_batch_size`（1..=128、既定 64）— `recvmmsg`/`sendmmsg` と multishot 提供バッファ数
3. **quiche 低レベル sans-IO 経路を維持しつつ、受信を `IORING_OP_RECVMSG` multishot 化**
   - provided buffers + `io_uring_recvmsg_out` レイアウトで peer アドレス / GRO cmsg / payload をゼロコピーで quiche へ
   - `PROXY_ALLOWED_OPCODES` に `RECVmsg` / `provide_buffers` / `remove_buffers` を追加
   - reactor / multishot 不可時は従来の POLL + `recvmmsg` フォールバック

## 改修内容

- `src/config.rs` / `Http3ServerConfig` / `entry.rs`（TOML → ワーカーへ正しい輸送パラメータを渡す）
- `src/udp/socket.rs`（バッチ幅ランタイム化、既定 64）
- `src/runtime/uring/{ring,executor,udp_recv}.rs`（multishot 受信）
- `src/http3_server.rs`（CC 適用、multishot メインループ）
- `examples/config.toml` / `contrib/config/config.toml` / README / README.ja

## 受け入れ条件

- `cargo build`（full / default / no-default-features / 主要 features）で warning なし（正当な `dead_code` allow は理由付きのみ）
- HTTP/3 関連ユニット・統合・E2E が通る
- Docker `--net=host` で GSO/GRO on/off の双方が動作
- tools/perf で HTTP/3 代表 1 構成のスループットを記録

## 依存・リスク

- multishot RECVMSG は kernel 6.0+。それ未満ではフォールバック
- provided buffer 枯渇（ENOBUFS）時は再提供 + rearm
