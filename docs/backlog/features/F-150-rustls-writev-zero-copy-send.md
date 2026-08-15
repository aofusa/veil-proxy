# F-150: rustls 送信経路のゼロコピー化（`writev(2)` 直結 + 静的配信のキャッシュ適用）

- 優先度: P1
- 状態: 完了
- 関連: `docs/perf/README.md`「残存ボトルネック（未対応）」の 1 件目、F-146、B-27、F-59

## 背景（何を解こうとしているか）

`docs/perf/README.md` の FreeBSD ネイティブ計測で **未対応として残っていた 2 件**のうちの 1 件:

> **大きな応答（54KB）の TLS スループットが約 1.2 GB/s で頭打ち**（nginx は 2.55 GB/s）。
> 暗号処理自体は 2 コアで 15.6 GB/s 相当あるので暗号律速ではなく、TLS 送信経路の
> コピー回数が効いていると見られる。

対 nginx 比は HTTP/1.1 TLS・54KB で **0.44**、HTTP/2 TLS・54KB で **0.52**。
FreeBSD では kTLS が大きな応答で不利（software kTLS のレコード単位ディスパッチ）と
判明済みで、**推奨構成は `ktls_enabled = false`＝ユーザ空間 rustls 経路**である。
つまり FreeBSD の大レスポンス性能はこの rustls 送信経路がそのまま支配する。

## 調査（コードリーディングで特定した事実）

### 事実 1: レスポンスごとに暗号文の全コピー + `malloc` が発生している

`src/simple_tls.rs`（非 Linux / kTLS 無効の本番経路）と `src/ktls_rustls.rs`
（Linux の rustls フォールバック経路）の書き込みは、いずれも次の形をしている。

```rust
while conn.wants_write() {
    let mut write_buf = Vec::new();          // ← リクエストごとのヒープ確保
    conn.write_tls(&mut write_buf)?;         // ← 暗号文チャンク列 → Vec への全コピー（+ 再確保）
    let mut written = 0;
    while written < write_buf.len() {
        match raw_write(fd, &write_buf[written..]) { ... }   // ← write(2) でカーネルへコピー
    }
}
```

rustls の `ConnectionCommon::write_tls` は内部の `ChunkVecBuffer`（TLS レコード単位の
`Vec<u8>` のデック）を `wr.write_vectored(&[IoSlice; ≤64])` **1 回**で吐き出す
（`rustls-0.23/src/vecbuf.rs::write_to`）。書き込み先が `Vec<u8>` だと、
この `write_vectored` は **暗号文をまるごと `Vec` へ memcpy** し、さらに容量拡張の
`realloc` コピーが乗る。54KB 応答（16KB レコード × 4）では
**1 レスポンスあたり 54KB の余分な memcpy + malloc/free 1 組**である。

これは AGENTS.md「ホットパス絶対規則」の
**「メモリアロケーションは、パフォーマンス上必要である場合を除いて一切禁止」**
**「ゼロコピーを徹底する」** に正面から反している。

### 事実 2: ヘッダとボディで TLS フラッシュが 2 回起きている

`SimpleTlsServerStream::write_all_vectored`（F-59、平文経路では 1 回の scatter-gather
送出）は **rustls モードでは 2 回の `write_all` へフォールバック**する。
`write_all` は 1 回ごとに上記のフラッシュループを回すため、
ヘッダとボディで **`write(2)` が 2 回**発行される。ヘッダは数百バイトしかないので、
これは「小さな TLS レコード 1 個ぶんの syscall」を毎レスポンス払っていることになる。

### 事実 3: HTTP/1.1 の静的配信はリクエストごとにファイルを読み直している

`src/proxy.rs::handle_sendfile_userspace`（kTLS 無効・非平文＝**FreeBSD の推奨構成**）は

```rust
while offset < target_end {
    let read_buf = buf_get();
    let (res, mut returned_buf) = file.read_at(read_buf, offset).await;   // ← pread(2)
    ...
    tls_stream.write_all(write_buf).await
}
```

と、**リクエストごとに `pread(2)`** でファイルを読み直す。F-146（静的コンテンツ
キャッシュ）は「HTTP/1.1 は `sendfile(2)`/kTLS のゼロコピー経路だから対象外」として
HTTP/2・HTTP/3 にしか適用していないが、**kTLS を無効にした FreeBSD の HTTP/1.1 は
そのゼロコピー経路に乗らない**ため、この前提が崩れている。
（`src/runtime/io.rs::File::read_at` は「コールドパスのみ」と書かれた**同期 `pread`**
であり、ホットパスから呼ばれている点も規則違反。）

## 改修内容

### 改修 1（本命）: rustls の暗号文チャンクを `writev(2)` で直接カーネルへ渡す

新規モジュール **`src/tls_writev.rs`** に、fd を保持し `io::Write` を実装するライタを置く。

- `write_vectored(&[IoSlice])` → `libc::writev(fd, iov, iovcnt)`（Windows は `WSASend`）。
- `write(&[u8])` → `libc::write`（rustls は `write_to` で必ず `write_vectored` を使うため
  実質未使用だが、トレイト契約として実装する）。
- 部分書き込みは戻り値をそのまま返す。rustls 側 `write_to` が `consume(used)` して
  残りをキューに保持するため、次の `write_tls` が続きを送る。
- `EAGAIN`/`EWOULDBLOCK` は `Err(WouldBlock)` を返す。`write_to` はこのとき
  `consume` を行わないので**データは 1 バイトも失われない**。呼び出し側は
  `writable().await` の後に `write_tls` を再実行する。

呼び出し側の共通ヘルパ `flush_tls(fd, conn, stream)` を同モジュールに置き、
`simple_tls.rs`（サーバ／クライアント／ハンドシェイク）と `ktls_rustls.rs`
（rustls フォールバック経路）の **全 10 箇所**を置換する。

これにより 1 レスポンスあたり
**暗号文の memcpy 1 回と malloc/free 1 組が完全に消える**（syscall 数は不変）。

### 改修 2: ヘッダ + ボディを 1 回のフラッシュへ合流させる

`write_all_vectored` の rustls 経路を「**両方の平文を `conn.writer()` へ積んでから
1 回だけフラッシュする**」形に変える。rustls は post-handshake では `writer().write()`
の時点でレコード化・暗号化して内部キューへ積むだけなので、両方積んでから
`flush_tls` を 1 回呼べば **`writev(2)` 1 回**で送出できる。

- rustls の送信バッファ上限（既定 64KB）に当たって `writer()` が部分受理した場合は、
  その時点でフラッシュして続きを積む（`write_all` 相当のループ）。挙動は不変。
- ヘッダ + ボディが 64KB 未満の一般的なケースでは **`write(2)` 2 回 → `writev(2)` 1 回**。

### 改修 3: HTTP/1.1 ユーザ空間 TLS 静的配信に静的コンテンツキャッシュを適用

`handle_sendfile_userspace` の非平文（rustls）経路で、
`[static_file_cache]` が有効かつ `file_size <= max_file_size_bytes` のとき:

1. `cache::content_cache` を**ヒット限定**で引く（新規 `get_cached`）。ヒットしたら
   `Bytes::slice()`（参照カウントのみ・コピーなし。Range 対応）を 1 回書くだけで完了。
2. ミス時は**既に開いている `File` の fd** から `read_exact_at` で全体を 1 回読み、
   `Bytes` 化して `insert`（新規）してから書く。
   - `cache::get_or_load` は `std::fs::read(path)` で**開き直す**ため使わない。
     FreeBSD の capability mode（`cap_enter`）下では絶対パス open が禁止であり、
     既存 fd を使う方が安全かつ syscall も少ない（open/close が増えない）。

条件を満たさない場合（キャッシュ無効・上限超過・巨大ファイル）は
**既存のチャンク読み出しループのまま**（メモリ上限の担保）。既定はオフなので
既定挙動は完全に不変。

## 期待効果

| 経路 | 削減されるもの |
|---|---|
| 全 rustls 送信（H1/H2/H3 バックエンド/L4 TLS 終端・全 OS） | 暗号文 memcpy 1 回 + malloc/free 1 組 / 送信 |
| ヘッダ + ボディ応答 | `write(2)` 2 回 → `writev(2)` 1 回 |
| HTTP/1.1 静的配信（キャッシュ有効時） | `pread(2)` 1〜N 回 + プールバッファ往復 → 0 |

## テスト

- 単体（`src/tls_writev.rs`）:
  - `writev` ライタが部分書き込み・`WouldBlock`・複数 `IoSlice` を正しく扱うこと
    （`socketpair` で送信バッファを意図的に埋めて検証）。
  - 送出バイト列が「1 本の `Vec` に集めてから `write` した場合」と**完全に一致**すること。
- 単体（`src/cache/content_cache.rs`）: `get_cached` がミス時にロードしないこと、
  `insert` の上限・mtime 再検証が既存 `get_or_load` と一致すること。
- 統合: rustls 経路で 54KB / 3B / Range / keep-alive 連続リクエストのレスポンスが
  バイト単位で正しいこと（TLS レコード分割の境界をまたぐ検証）。
- E2E（`full` および `full,epoll`）: 静的配信・プロキシ・HTTP/2・L4 TLS 終端の
  既存シナリオに加え、`static_file_cache` を有効にした HTTP/1.1 大ファイル配信と
  Range リクエストを追加する。

## 検証結果

### テスト

- `cargo test --lib --features full`: **853 passed / 0 failed**
  （`tls_writev` の socketpair 実 fd 単体テスト 5 件 + `content_cache` の `get_cached`/
  `insert_bytes` 単体テスト 4 件を追加）
- `cargo test --test integration_tests --features full`: **54 passed / 0 failed**
  （実 TCP + 実 rustls クライアントで 54KB / 3B / 0B のヘッダ+ボディがバイト一致することを検証）
- E2E `full`（io_uring）/ `full,epoll`（reactor）: いずれも **544 passed / 0 failed**
  （`static_file_cache` を有効にした HTTP/1.1 の 2 回連続 GET と、TLS レコード境界を
  またぐ Range リクエストの E2E 2 件を追加）
- FreeBSD 14.3 amd64 実機（`full-freebsd`）E2E: 543 passed / 1 failed
  （失敗は B-61 の先行バグ。改修前コミットで 3/3 失敗・改修後 2/3 失敗の A/B で確認済み）
- `cargo build --features full` / `--no-default-features` / 各 feature 単体 /
  `cargo clippy --features full --all-targets`: いずれも **warning 0**

### 性能（FreeBSD 14.3 amd64 / QEMU+KVM、54KB、対 nginx 比）

`docs/perf/README.md`「F-150/F-151 後の FreeBSD 計測」節を参照。

| シナリオ | 改修前の対 nginx 比（aarch64） | 改修後の対 nginx 比（amd64） |
|---|---|---|
| HTTP/1.1 TLS | 0.44 | **0.97** |
| HTTP/2 TLS | 0.52 | **1.05** |
| HTTP/1.1 proxy | 0.72 | **1.02** |
| HTTP/2 proxy | 0.94 | **1.42** |

**注意**: 改修前の計測は aarch64 / QEMU+HVF、改修後は amd64 / QEMU+KVM であり、
**絶対値の直接比較はできない**。有効なのは同一実行内で併走させた nginx との比である。

### Linux 退行確認

`h2_1_ktls_0_lb_kernel_ofc_1`（3 反復、全 Non-2xx = 0）で **退行なし**。
同時計測の nginx 比は veil_glibc HTTP/1.1 1.46×・HTTP/2 1.19×、
veil_musl 1.44×・1.20× で、過去計測（1.41〜1.45 / 1.17〜1.19 / 1.43 / 1.15）と
同等かわずかに良い。詳細は `docs/perf/README.md`。
