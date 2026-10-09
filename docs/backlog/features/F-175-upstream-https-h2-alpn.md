# F-175: 上流 HTTPS の HTTP/2（ALPN）

**状態: 対応中（feat/v080-limitations）**

B-83 で上流 TLS の ALPN を `http/1.1` のみにした（h2 を提示しながら HTTP/1.1 を送って 502 になっていたため）。
本チケットで TLS 上の HTTP/2 上流を実装する。

## 設計

- 上流ごとの設定 `http2 = "auto" | "on" | "off"`。**既定は `"auto"`**（ALPN に `h2, http/1.1` を提示し、
  ネゴシエーション結果で HTTP/2（F-174）と HTTP/1.1 を切り替える）。後方互換は考慮しない（v0.8.0 の方針）。
- `ClientConfig` は ALPN 違いの系統を設定ロード時に作って `Arc` で持つ（要求ごとに作らない）。
- HTTP/1.1・HTTP/2・HTTP/3 で受けた HTTPS 上流すべてに適用する。
