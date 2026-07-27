# B-54: 非 kTLS ビルドで HTTP/3 → TLS バックエンドのプロキシが close_notify 無しの EOF で 502 になる

**状態: 修正済み（2026-07-27）**

## 事象

OpenBSD 7.9 amd64（QEMU VM、`--no-default-features --features full-openbsd`）の E2E で、
**HTTP/3 の Proxy ルートだけが 502** を返す。静的配信（File ルート）の HTTP/3 は通る。

```
---- test_http3_buffering_full_proxy_body_intact stdout ----
HTTP/3 buffering full proxy status=502 body_len=11
assertion `left == right` failed: buffering full proxy should return 200
  left: 502
 right: 200

---- test_http3_wasm_integration stdout ----
assertion `left == right` failed: HTTP/3 /wasm/ should return 200
  left: 502
 right: 200
```

veil のログ:

```
WARN [src/http3_server.rs:2086] [HTTP/3] Async backend proxy error:
  peer closed connection without sending TLS close_notify:
  https://docs.rs/rustls/latest/rustls/manual/_03_howto/index.html#unexpected-eof
```

負荷の低いホストで HTTP/3 系のみ再実行しても **再現する**（他の HTTP/3 のタイムアウト系
失敗は負荷起因のフレークで、静かなホストでは解消した）。

## 原因

`src/http3_server.rs` の `proxy_to_tls_backend_async` は **kTLS 版と非 kTLS 版の 2 つ**が
`cfg(veil_ktls)` / `cfg(not(veil_ktls))` で分かれている。バックエンド応答の読み取りが
両者で異なっていた。

kTLS 版（`#[cfg(veil_ktls)]`）— **UnexpectedEof を正常終了として扱う**:

```rust
loop {
    match std::io::Read::read(&mut tls, &mut buf) {
        Ok(0) => break,
        Ok(n) => response.extend_from_slice(&buf[..n]),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,  // ← 許容
        Err(e) => return Err(e),
    }
}
```

非 kTLS 版（`#[cfg(not(veil_ktls))]`）— **`read_to_end` がそのまま伝播**:

```rust
tls.read_to_end(&mut response)?;   // ← close_notify 無しの EOF がエラーになる
```

rustls は相手が `close_notify` を送らずに TCP を閉じると
`io::ErrorKind::UnexpectedEof`（"peer closed connection without sending TLS
close_notify"）を返す。HTTP/1.1 のバックエンドが `close_notify` を送らずに閉じるのは
ごく一般的なため、非 kTLS ビルドでは HTTP/3 → TLS バックエンドのプロキシが
**恒常的に 502** になる。

## 影響範囲

`veil_ktls` は `feature = "ktls"` かつ `target_os` が linux / freebsd のときのみ立つ
（AGENTS.md）。したがって影響を受けるのは:

- **OpenBSD**（kTLS 非対応。`full-openbsd` も該当）← 実際に検出された環境
- **macOS**（kTLS 非対応）
- **Windows**（kTLS 非対応）
- Linux / FreeBSD でも **`ktls` feature を外したビルド**（`--no-default-features` 等）

Linux の既定ビルドは `default` に `ktls` が入っているため kTLS 版が使われ、
この不具合は表面化しない。**そのため今まで気付かれなかった。**

## 修正

非 kTLS 版の `read_to_end` を、kTLS 版と同じ「`UnexpectedEof` は break」ループへ置換した。
併せて不要になった `Read` の import を整理。

## 検証

OpenBSD VM で HTTP/3 系のみ実行:

- 修正前: `76 passed / 2 failed`（`test_http3_buffering_full_proxy_body_intact` /
  `test_http3_wasm_integration` がいずれも 502）
- 修正後: **`78 passed / 0 failed`**

## 関連

- B-50（FreeBSD で HTTP/3 の UDP が bind されない。別件・修正済み）
- B-52（OpenBSD の WASM。別件・修正済み。これを直したことで
  `test_http3_wasm_integration` が実行されるようになり本件が見えた）
- F-126（FreeBSD kTLS）、F-122（OpenBSD は ring + simple_tls フォールバック）
