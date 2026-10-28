# B-221: HPACK のエンコーダの動的表がピアの `SETTINGS_HEADER_TABLE_SIZE` をそのまま上限にして、1 接続で無制限に育つ

**状態: 修正済み（v0.10.0）**

## 出典

2026-10-27 のセキュリティレビュー（v0.9.2 のコード読解）。

## 事象

- `http2/connection.rs::handle_settings` は `0x1`（`SETTINGS_HEADER_TABLE_SIZE`）を `self.hpack_encoder.set_max_table_size(value as usize)` と
  **そのまま**自分のエンコーダの動的表の上限にする。`http2/upstream_mux.rs:677`（上流の HTTP/2 の多重化）も同じ。
- `http2/hpack/encoder.rs::encode`（L143 付近）は sensitive でない全ヘッダ（`date`・`etag`・`content-length`・`set-cookie` など値が毎回違うもの）を
  毎回 `dynamic_table.insert` する。
- クライアントが `SETTINGS_HEADER_TABLE_SIZE = 0xffffffff` を送ってから同じ接続で要求を繰り返すと、**接続ごとに動的表が無制限に育つ**
  （1 要求あたり数十〜数百バイト。1 接続で 100 万要求なら 100MB 超。接続数で倍加）。上流の多重化は悪意ある上流で同じ。
- nghttp2・h2（Rust）はピアの値と自前の上限（4096）の小さい方を使う。

## 再現手順（案）

1. h2 のクライアントで接続し、`SETTINGS` に `SETTINGS_HEADER_TABLE_SIZE = 0xffffffff` を入れて送る。
2. 同じ接続で `GET /`（上流が `date`・`etag` を返すルート）を 10 万回送る。
3. veil の RSS が要求数に比例して増え、接続を閉じるまで戻らないことを確かめる。

## 改修案

- `set_max_table_size(min(value, LOCAL_MAX_ENCODER_TABLE))`（`LOCAL_MAX_ENCODER_TABLE` = 4096）に丸める。デコーダの
  `max_allowed_table_size` はすでに自前の上限を持つ。上流の多重化も同じ。
- 値が毎回違うヘッダ（`date`・`content-length`・`etag` など）を表に入れない方が効率もよい（任意。性能の交互 A/B で確かめる）。

## 受け入れ条件

- 単体: `SETTINGS_HEADER_TABLE_SIZE = u32::MAX` の後に 10 万要求を流しても動的表のサイズが 4096 を超えない。
- fuzz: HTTP/2 の接続状態機械（SETTINGS → HEADERS → CONTINUATION → DATA の列）に RSS の上限を assert する target（F-279）。
- h2spec・性能の交互 A/B（HTTP/2 の 3B・54KB）が変わらないこと。

## 修正（v0.10.0）

- エンコーダの動的表の上限を、ピアの `SETTINGS_HEADER_TABLE_SIZE` と自前の上限 4096（`hpack::encoder::MAX_ENCODER_TABLE_SIZE`）の小さい方に丸める。
  `HpackEncoder::new`・`set_max_table_size` の中で丸めるので、下流の接続（`http2/connection.rs`）・上流の多重化（`http2/upstream_mux.rs`）の両方に効く。
- 単体: `SETTINGS_HEADER_TABLE_SIZE = u32::MAX` の後に値が毎回違うヘッダで 10 万回エンコードしても表が 4096 を超えない
  （`test_peer_huge_table_size_is_clamped`）。

## 関連

- [F-279](../features/F-279-security-test-gaps.md)、[B-146](B-146-h2-buffered-request-body-unbounded.md)（HTTP/2 の別の無制限）
