# B-296: `buffering = "adaptive"` が応答ではなく要求の Content-Length で判定し、GET を全部バッファしている

**状態: 未修正（2026-11-05 起票。コードレビューで発見）**

## 事象（コードの読み取り）

- `src/proxy/forward.rs:881-884` は `buffering_config.should_buffer(Some(content_length))` に**要求**の Content-Length を渡している
  （`src/buffering/config.rs:147-159`）。GET（長さ 0）は「小さい」とみなされ、常に全体をバッファする経路へ入り、kTLS・平文の splice を使わない。
- 文書 `docs/guide/compression-cache.md` の「Adaptive Mode Behavior」（:414-417、ja も）は「大きい応答はストリーミングし kTLS の splice を保つ」と書いている。

## 改修案

- 判定は上流の応答ヘッダを読んでから、応答の Content-Length（chunked・不明はストリーミング）で行う。HTTP/2・HTTP/3 の adaptive の判定も同じ値を見ているか確かめ、
  そろえる。要求の本文のバッファ（`max_request_buffer`）とは別に扱う。
- 試験: adaptive で小さい応答はバッファ（`veil_proxy_buffering_used_total` が増える）、閾値を超える応答はストリーミング（増えない・splice の経路）。

## 関連

- [F-292](../features/F-292-ktls-h1-zero-copy-same-as-plain.md)、[B-273](B-273-metrics-recorded-on-some-protocols-only.md)
