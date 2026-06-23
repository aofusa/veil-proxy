# F-26: HTTP/2 ヘッダ/ボディのヒープ割り当て排除（bytes クレート化）

## 概要

`src/http2/stream.rs` でストリームごとにヘッダ辞書やボディブロックが引き起こしているヒープアロケーションを、`bytes` クレートを活用したゼロコピー共有アーキテクチャに変換する。

## 現状

```rust
pub struct Stream {
    pub request_headers: Vec<HeaderField>,  // Vec<(Vec<u8>, Vec<u8>)>
    pub request_body: Vec<u8>,              // 累積ボディ
    pub response_headers: Vec<HeaderField>,
    pub response_body: Vec<u8>,
}
```

- ストリームごとに `Vec<u8>` が個別にアロケートされる
- DATA フレームのボディが `extend_from_slice` でコピーされる
- リクエスト処理中のアロケーション回数が多い

## 改修内容

### request_body / response_body の BytesMut 化

```rust
use bytes::{Bytes, BytesMut};

pub struct Stream {
    pub request_body: BytesMut,   // ゼロコピー追記・分割
    pub response_body: BytesMut,
}
```

`recv_data()` の `self.request_body.extend_from_slice(data)` はそのまま維持し、
`freeze()` でゼロコピーの `Bytes` に変換してバックエンドへ渡す。

### プールからの BytesMut 取得

`pool.rs` の `BUF_POOL` から `BytesMut::with_capacity(BUF_SIZE)` を取得し、フレーム処理終了後にプールへ返却する。

## 改修案

1. `Stream::new()` で `request_body: BytesMut::with_capacity(4096)` に変更
2. `recv_data()` で `self.request_body.extend_from_slice(data)` は変更なし（BytesMut も同メソッドを持つ）
3. バックエンドへの転送時: `let body = self.request_body.split().freeze()` でゼロコピー

## 受け入れ条件

- [ ] `Stream.request_body` / `response_body` が `bytes::BytesMut` になっている
- [ ] バックエンドへの書き込みでコピーが発生しない（`Bytes::freeze()` 使用）
- [ ] `cargo test --features "full"` が通る
- [ ] `cargo clippy --features "full"` が通る

## 依存・リスク

- bytes クレートはすでに依存関係に含まれている（`bytes = "1.11.0"`）
- HPACK デコーダ（`src/http2/hpack/`）の出力型変更が必要になる可能性あり

## 優先度

P2

---

## 完了メモ（2026-06-23）

`Stream.request_body` / `response_body` は `bytes::BytesMut` 化済みだった。加えて HTTP/2 → バックエンド転送経路（`src/proxy.rs`）に残っていた **`stream.request_body.clone()`（BytesMut の deep clone = ボディ全体の memcpy）を排除**し、`std::mem::take(&mut stream.request_body).freeze()` で所有権ごとゼロコピーに変換（`Bytes` 参照カウント共有）。

アーキテクチャ上の注記: Veil の HTTP/2 はプロキシで終端し、バックエンドへは HTTP/1.1 として**再シリアライズ**するため、DATA フレームのボディは最終的に HTTP/1.1 リクエストバッファへ 1 度はまとめられる（プロトコル変換のため不可避）。本変更はその経路上の**冗長なコピー**を除去するもの。
