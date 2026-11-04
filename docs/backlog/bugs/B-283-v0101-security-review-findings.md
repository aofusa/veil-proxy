# B-283: v0.10.1 の改修のセキュリティレビューの所見（h2 の上流の圧縮の上限・静的圧縮の増幅・Wasm の固定応答のヘッダ等）

**状態: 修正済み（v0.10.1）**

## 出典

v0.10.1 の改修（B-266〜B-282・F-285〜F-287）の差分を Fable 5.1 のサブエージェントで読み取りのみのセキュリティレビューにかけた
（報告は `docs/artifacts/security/v0101-fable-review.md`）。

## 所見と対応

| 重さ | 所見 | 対応 |
|---|---|---|
| Medium | 上限（B-281）が h2 の上流の 3 経路に無い: HTTP/2 → h2c/h2 の上流の圧縮（`h2_relay_h2_streaming` の `pending`）、HTTP/1.1 → ALPN の h2 の上流の圧縮（`proxy_h1_via_https_h2`）、h2c の応答をまとめる `upstream_mux::collect_response`。長さを持たない圧縮対象の応答で上流が終わるまでメモリを使う | 3 経路とも `MAX_COLLECTED_RESPONSE_BODY`（64 MiB）を超えたら 502（`collect_response` は再送しない `Reset`） |
| Low | HTTP/1.1 の静的配信の圧縮（B-269）は、圧縮結果のキャッシュが無いと要求ごとにファイル全体を読み込んで圧縮する（並行する要求で CPU・メモリが増幅する。上限は 64 MiB だけ） | その場で圧縮する静的ファイルを 8 MiB 以下にする（`static_response::MAX_STATIC_COMPRESS_SIZE`。全プロトコル。超えるものは圧縮せず `sendfile`・Range に応える）。文書に書いた |
| Low | `write_content_range` の 64 バイトの配列は u64 の桁数の最大（68 バイト）に足りない（10^18 バイト以上のファイルでだけ。ネットワークからは起こせない） | 80 バイトにする |
| Low | `compress_body` が失敗すると元の本文を返すのに、呼び出し側は先に `Content-Encoding` を付ける | 変更なし: 圧縮の水準は設定の検証で範囲内に限られ、書き込み先は `Vec` なので失敗しない（失敗は確保の失敗＝abort だけ）。0.10.0 と同じ |
| Low | Proxy-Wasm の `proxy_send_local_response` のヘッダの名前を小文字にしていない（HTTP/2・HTTP/3 のクライアントが PROTOCOL_ERROR）。HTTP/1.1 は名前・値の CR・LF を確かめずに書く | ホスト関数で名前を小文字にし、CR・LF・NUL を含むもの・空の名前を捨てる。範囲外のステータス（100〜599 の外）は 500 にする（`u16` への切り詰めをしない）。単体試験 `local_response_headers_are_lowercased_and_injection_free` |
| Info | `buffering = "full"` でディスクへ溢れた本文は Wasm の応答本文の段を通らない（文書化済みのフェイルオープン） | 変更なし（`docs/guide/wasm.md` に記載済み） |
| Info | kTLS の splice 経路の head の作り直し（B-275）が、Alt-Svc・Server を付ける既定の構成で要求ごとに `Vec` を確保する | スレッドごとのバッファへ書く（`render_response_head_into`。64 KiB を超えて育ったものは戻さない） |

確認して問題の無かった領域（Range・条件付き要求・ETag の形・HEAD・切り詰めの見せ方・B-277・見回りのタイマー・SSE の除外・
Server/Alt-Svc の削除・メトリクスのラベル・ブロッキングの追加の有無）はレビューの報告に列挙した。

## 関連

- [B-281](B-281-unbounded-response-collection-for-compression.md)、[B-269](B-269-h1-static-files-not-compressed.md)、[B-275](B-275-ktls-splice-raw-response-head.md)
