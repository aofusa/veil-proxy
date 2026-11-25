# B-503: protocol-parity の後、h2spec の full で 3 件落ちていた（h2c の 3.5/2・4.2/3）のを nightly が見逃していた

**状態: 修正済み（feat/httparena-perf。2026-11-26 起票・修正）**

## 出典

feat/httparena-perf の最終の検証で、aofish の Docker で `tools/container_security` の h2spec を `H2SPEC_FULL=1 H2SPEC_STRICT=1` で
回して見つけた（86297922）。main の nightly（`H2SPEC_FULL=1`、厳格でない）でも 1f8468dd（feat/protocol-parity のマージ）から同じ 3 件が
落ちていた（run 45529601956 の `h2spec_tls_full: failed`・`h2spec_h2c_full: failed`）。192edb00 の nightly までは 146 件すべて通っていた。

```
TLS:  4. HTTP Frames / 4.2. Frame Size
  × 3: Sends a large size HEADERS frame that exceeds the SETTINGS_MAX_FRAME_SIZE
       Expected: GOAWAY Frame (Error Code: FRAME_SIZE_ERROR) / Connection closed
         Actual: DATA Frame (length:31, flags:0x01, stream_id:1)
h2c:  3.5. HTTP/2 Connection Preface
  × 2: Sends invalid connection preface
       Expected: GOAWAY Frame (Error Code: PROTOCOL_ERROR) / Connection closed
         Actual: Error: unexpected EOF
      4.2. Frame Size / × 3（TLS と同じ）
```

## 原因

| 失敗 | 原因 |
|---|---|
| h2c の 3.5/2 | B-490 で平文のリスナーの判別（`proxy::detect`）を「メソッドのトークン + SP」なら HTTP/1.1 にした。h2spec の `INVALID CONNECTION PREFACE\r\n\r\n` はトークン + SP で始まるので HTTP/1.1 の 400 を返し、h2spec はそれをフレームとして読んで `unexpected EOF` になった（B-102 の GOAWAY(PROTOCOL_ERROR) の経路に届かない） |
| 4.2/3（TLS・h2c） | h2spec は 4,000 バイトのヘッダを 5 つ付けた約 17.5 KB の HEADERS を送る（相手の SETTINGS_MAX_FRAME_SIZE を RFC の既定の 16384 と決め打ちしている）。veil の既定は 65536 なのでフレームは上限の内で、veil は要求ヘッダの上限で 431 を返す（31 バイトの DATA は `Request Header Fields Too Large`）。これは B-482 で揃えた振る舞いで正しい。以前は HPACK のデコーダの上限が 16 KB のままで、約 20 KB のヘッダリストを COMPRESSION_ERROR で接続ごと閉じていたので、たまたま通っていた（h2spec は接続が閉じれば合格にする） |
| nightly が見逃した | `h2spec_run.sh` は full の失敗で `H2SPEC_STRICT=1` のときだけ止まり、レポートにはゲートの `h2spec_tls_gate: ok` の行が残る。集計（`lib/report.sh`）は `: ok` の行で passed に数え、`h2spec_tls_full: failed` は失敗の印（`: fail` の後ろが空白か行末）に当たらない |

## 修正

- `proxy::detect::looks_like_http1_request_line`: 要求行の形（`メソッド SP 要求先 SP HTTP-version`、RFC 9112 §3。要求先は SP を含まない）に
  沿って、2 つ目の SP が見えていればその後ろが `HTTP/1.` と食い違わないことも確かめる。`INVALID CONNECTION PREFACE`・要求先に SP を含む行・
  `PRI * HTTP/2.0` で始まる壊れたプリフェースは HTTP/1.1 にせず、GOAWAY(PROTOCOL_ERROR) で閉じる（B-102 の作法）。拡張メソッドの要求行
  （B-490）は従来どおり HTTP/1.1。判別は接続ごとに 1 回、覗いた 24 バイトの中だけ（ホットパスは無変更）。
- `tools/container_security/fixtures/veil-config.toml`: `[http2] max_frame_size = 16384`（RFC の既定）。4.2/3 の HEADERS が本当に上限を
  超え、veil の FRAME_SIZE_ERROR を確かめる試験になる。
- `h2spec_run.sh`: full が落ちたら `FAIL h2spec: ...` の行を書く（集計が失敗に数える）。終了コードは従来どおり（`H2SPEC_STRICT=1` のときだけ 1）。
- 文書: `configuration.md`（英日）の `listen_cleartext` に判別の規則を書いた。`tools/container_security/README.md`。

## 試験

- 単体: `proxy::detect::request_line_tests::version_after_the_target_must_be_http1`。
- 自己完結型 E2E: `protocol_parity_tests::b503_cleartext_invalid_preface_gets_goaway`（3 つの入力に GOAWAY(PROTOCOL_ERROR) の 17 バイトが
  返って FIN で閉じる。GET・PROPFIND の HTTP/1.1 は 200）。直す前の木では HTTP/1.1 の 400 を受けて落ちる。
- h2spec（aofish の Docker、`H2SPEC_FULL=1 H2SPEC_STRICT=1`）で TLS・h2c の full が全件通ること（結果はこの後に書く）。

## 影響

- P2。h2c で不正なプリフェース・HTTP/1.1 の文法に合わない要求行を送った相手に、HTTP/2 の GOAWAY ではなく HTTP/1.1 の 400 を返していた
  （どちらも接続は閉じる）。4.2/3 は veil の不具合ではない（試験の前提の違い）。
- nightly の full の失敗を 4 日（2026-11-22〜25）見逃した。

## 関連

- [B-102](B-102-h2c-invalid-preface-rst.md)・[B-482](B-482-request-header-limit-differs-by-protocol.md)・[B-490](B-490-http1-rejects-extension-methods.md)・
  [B-502](B-502-url-advisory-nightly-cargo-audit.md)（同じ nightly を落としていた依存の勧告）
