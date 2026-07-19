# F-124: HTTP/3 quiche 輻輳制御・バッチ設定 + 低レベル API + io_uring RECVMSG

**ステータス: 完了（2026-07-19）**

## 機能説明

1. **`[http3]` で quiche `Config` の輻輳制御 / Pacing / HyStart を設定可能にする**
   - `cc_algorithm`（既定 `bbr`）、`pacing`（既定 true）、`max_pacing_rate`、`hystart`（既定 true）
2. **UDP バッチ幅を既定 64 にし、設定可能にする**
   - `mmsg_batch_size`（1..=128、既定 64）— `recvmmsg`/`sendmmsg` バッチ幅
3. **quiche 低レベル sans-IO + `IORING_OP_RECVMSG` 受信経路**
   - 先頭パケット: `IORING_OP_RECVMSG` + `IORING_RECVSEND_POLL_FIRST`（POLL_ADD+同期 recvmsg の二重往復を排除）
   - 継続 drain: `recvmmsg` で batch 件一括
   - `PROXY_ALLOWED_OPCODES` に `RECVMSG` / `PROVIDE_BUFFERS` / `REMOVE_BUFFERS` を追加（将来の true multishot 用含む）
   - reactor / 初期化失敗時は従来の POLL + `recvmmsg` フォールバック

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
