# B-297: ゼロコピー（sendfile・splice・kTLS）・圧縮・キャッシュ・L4 の文書が実装と食い違っている

**状態: 修正済み（2026-11-05）**

## 食い違い（f8dc80de。英日とも）

ゼロコピー（`docs/artifacts/v0110/review-04-ktls-zero-copy.md` §2.2）:
- `compression-cache.md:254`（ja :252）「streaming のバッファリングでは kTLS のゼロコピーを保つ」→ キャッシュのあるルートの GET は splice しない（`forward.rs:289-327,903`）。
- `compression-cache.md:157`「圧縮した応答だけ sendfile を使わない」→ プロキシでは、圧縮が有効なルートに `Accept-Encoding` 付きで来た要求は全部 splice しない。
- `compression-cache.md:414-417` の Adaptive の説明（[B-296](B-296-adaptive-buffering-uses-request-length.md)）。
- `examples/config.toml:496-525`: `chunk_size_mode`・`per_stream_pipe_enabled` を kTLS だけと書いている（平文の splice も使う）。
- `examples/config.toml:2009`: HTTP/1.1 は静的コンテンツキャッシュの対象外と書いている（平文・rustls は使う）。
- `examples/config.toml:2221`: 圧縮が迂回するのは kTLS のゼロコピーだけと書いている（平文のゼロコピーも迂回する）。
- `l4-proxy.md:128`・`features.md:21`: L4 が splice・sendfile と書いている（L4 に sendfile は無い。BSD・macOS・Windows はコピー）。
- `performance.md:228-236`: HTTPS の上流（両端 kTLS でも）・WebSocket・HTTP/2 over kTLS はコピーすることを書いていない。

圧縮・静的配信（`docs/artifacts/v0110/review-06-precompressed.md` の D1〜D8）:
- D1: `library-kit.md:96`（ja :93）・`library-production.md:129`（ja）・`src/api/app.rs:460`・`crates/veil-kit/src/serve.rs:6` の rustdoc
  「HTTP/1.1 はファイルをそのまま送る」→ [B-269](B-269-h1-static-files-not-compressed.md) 以降は HTTP/1.1 も圧縮する。
- D2: `ServeOptions::static_file_cache` の rustdoc が HTTP/2・HTTP/3 だけと書いている。
- D3: SVG は既定で圧縮すると書いている（`compression-cache.md:119`・ja :117・`examples/config.toml:2254`・`library-production.md`）→ 実際は圧縮しない
  （[B-303](B-303-compression-defaults-feature-off-h3-clone.md)。コードを直すなら文書はそのまま）。
- D4: `preferred_encodings` を「この中から選ぶ」と書いているが制限になっていない（[B-302](B-302-accept-encoding-negotiation.md)。コードを直す）。
- D5: `examples/config.toml:1435`「ルートの `enabled = false` が `[http3]` より優先」→ コードはそうしていない。
- D6: 「8MiB を超えると sendfile」は HTTP/2・HTTP/3 では誤り（全体をメモリへ読む）。
- D7: `routing.md:332`「すべての応答に ETag」→ 416 には付かない。
- D8: `features.md:52` が F-169 のキャッシュを HTTP/2・HTTP/3 だけと書いている。
- リリースノートの Known limitations（`docs/releases/v0.10.1.md:118` の事前圧縮）は [F-294](../features/F-294-precompressed-static-siblings.md) の完了時に消す。

## 改修案

- コードを直すもの（D3・D4・B-296）はそのチケットと同じ PR で文書を直す。それ以外は本チケットで英日・`examples/config.toml`・rustdoc を直す。
- `tools/release/check-docs.sh` で拾えるもの（キー名の存在）は拾う。

## 修正

- D1〜D8 を英日の文書・`examples/config.toml`・rustdoc で直した（compression-cache・routing・performance・l4-proxy・features・library-kit・library-production、
  `App::serve_dir`・`veil-kit` の `serve`）。事前圧縮の行は F-294 に合わせて書き直した。Known limitations の行は v0.11.0 のリリースノートで消す。

## 関連

- [F-292](../features/F-292-ktls-h1-zero-copy-same-as-plain.md)、[F-294](../features/F-294-precompressed-static-siblings.md)、[B-269](B-269-h1-static-files-not-compressed.md)
