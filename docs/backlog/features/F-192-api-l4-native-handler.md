# F-192: L4（TCP/UDP）ネイティブハンドラ（API ライブラリ）

**状態: 進行中（feat/f188-api-library-vds-k8s）**

## 目的

`[[l4]]` リスナーに Rust 関数をカスタムハンドラとして登録する（`#[veil::l4_handler(name, protocol)]` または `App::l4_tcp/l4_udp`）。

## 設計

- feature `l4-handler`（`handler` + `l4-proxy`）。`[[l4]] handler = "name"`（ハンドラ指定時は `upstreams` 不要）。
- TCP: `async fn(&mut L4Connection) -> Result<(), L4Error>`、UDP: `async fn(L4Datagram) -> Option<Bytes>`（返信）。
- ハンドラ未指定のリスナーは従来の splice 経路のまま（コード経路・性能とも不変）。WASM network filter・TLS passthrough との併用は設定エラー。

## 受け入れ条件

- E2E: TCP エコー・UDP エコー、ハンドラ無しの既存 L4 E2E が通る。
