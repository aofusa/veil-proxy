# B-34: HTTP/3 quiche クライアントで応答タイムアウト

## 事象

`tools/container_security` の `http3-client`（quiche）が Veil の HTTP/3 エンドポイントへ GET `/` を送信すると、QUIC 接続後に `HTTP/3 response timeout` で失敗する。

## 再現手順

1. `veil:glibc`（`full`）を `tools/container_security/fixtures/veil-config.toml` で起動
2. ハーネスから `http3-client` を実行（`VEIL_HOST=veil-proxy` `VEIL_SNI=veil-proxy`）
3. `/results/http3_client_report.txt` にタイムアウトが記録される

同一条件下で HTTPS（TCP 443）は 200、UDP 443 到達性も OK。

## 影響

- container_security の `http3_probe` は TLS 生存フォールバックで合格するが、**本物の HTTP/3 応答は未検証**
- HTTP/3 経路のリグレッション検出が弱い

## 調査メモ

- ハーネス側の SNI/DNS は修正済み（ホスト名解決）
- `verify_peer(false)` で証明書検証は無効
- ベンチ `benches/http3.rs` と同系の quiche クライアント実装

## 改修案

1. Veil HTTP/3 サーバのリクエスト処理・ストリーム完了経路を調査（h3 FIN 未送出の可能性）
2. `http3-client` のイベントループ・`:status` 受信条件をサーバ挙動に合わせて調整
3. 修正後 `http3_client: ok` を必須化（フォールバックのみの合格を廃止）

## 関連

- F-90 container_security full features
- P-03 テストケース一覧

## 対応状況（完了）

**根本原因**: `http3-client` が `initial_max_streams_uni` 未設定（既定 0）のため、サーバが HTTP/3 制御ストリームを開く際に `StreamLimit` で接続が切断されていた。

**修正**:
- `http3-client` に `set_initial_max_streams_uni(100)` / `set_initial_max_stream_data_uni` を追加
- HTTP/3 を単一ワーカーに集約（QUIC 状態の SO_REUSEPORT 分散回避）
- ハンドシェイク直後の eager `init_h3`
- `http3_probe` の検証パスを `/`（200 応答）へ変更

**検証**: F-90 `http3_probe` — `http3_client: ok`（2026-07-08）