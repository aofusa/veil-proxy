# F-24: HTTP/2・HTTP/3 ゼロコピーパイプライン

## 概要

HTTP/2・HTTP/3 のデータパスに splice またはバッファ参照渡しによるゼロコピーストリーミングプロキシを実装する。

## 現状

- kTLS は TCP (HTTP/1.1) で最大の恩恵を受けている
- `http2/stream.rs` の `request_body: Vec<u8>` / `response_body: Vec<u8>` がデータを一度ユーザー空間でコピー・蓄積している
- カーネルで復号されたデータをユーザー空間でコピーすると kTLS の恩恵が半減する

## 改修内容

### HTTP/2 ストリーミングプロキシパス

- `http2/stream.rs` の `request_body` / `response_body` を `bytes::Bytes` / `bytes::BytesMut` に変換
- データフレーム受信時に `.extend_from_slice` → `split().freeze()` でゼロコピースライスを保持
- バックエンドへの書き込みは `Bytes` を参照渡し（clone はアーク参照カウントのみ）

### HTTP/3 ゼロコピーパス

- `http3_server.rs` の UDP バッファを quiche の `recv_body` → io_uring SEND に直結
- `IORING_OP_SPLICE` を使用してパイプ経由でカーネル空間コピーを排除

### splice ゼロコピー（kTLS 連携）

- kTLS ソケットからバックエンドソケットへの `splice(2)` パイプラインを実装
- `src/pool.rs` の `SPLICE_PIPE` スレッドローカルを活用

## 改修案

```rust
// http2/stream.rs
pub struct Stream {
    // 変更前
    pub request_body: Vec<u8>,
    pub response_body: Vec<u8>,

    // 変更後
    pub request_body: bytes::BytesMut,
    pub response_body: bytes::BytesMut,
}
```

## 受け入れ条件

- [ ] HTTP/2 ストリームのボディが `bytes::BytesMut` を使用している
- [ ] ゼロコピーパスでの throughput がベースラインより 20% 以上向上
- [ ] `cargo test --features "full"` が通る
- [ ] `cargo clippy --features "full"` が通る

## 依存・リスク

- F-28（カスタム io_uring 実装）に依存（`IORING_OP_SPLICE` 使用のため）
- bytes クレートはすでに依存関係に含まれている

## 優先度

P2（F-28 完了後に着手）

---

## 完了メモ（2026-06-23）

- **HTTP/1.1 + kTLS の splice ゼロコピー経路は実装済み**（`src/proxy.rs` の `proxy_http_request_splice` / `splice_body_transfer` / `splice_transfer_response_ktls`）。`SPLICE_F_NONBLOCK` のノンブロッキング splice を io_uring の readiness `.await` と組み合わせて駆動しており、ワーカーをブロックしない非同期パスになっている（スレッドローカル/ストリーム毎パイプ再利用）。kTLS の恩恵を最大化する経路はここ。
- **HTTP/2**: Veil は HTTP/2 を終端し HTTP/1.1 へ再シリアライズする終端型プロキシのため、フレーム単位の `IORING_OP_SPLICE` による端から端へのゼロコピーは適用できない。代わりに F-26 でボディの deep clone を排除し冗長コピーを削減した。
- **HTTP/3**: QUIC/UDP は quiche がユーザー空間で暗号化・再送制御を行う設計のため、TCP splice 相当のカーネルゼロコピーは原理的に不可。

end-to-end splice は終端型プロキシのアーキテクチャ上 HTTP/1.1+kTLS 経路に限定されるのが現実的な最適解であり、その経路は非同期・ゼロコピーで実装済み。
