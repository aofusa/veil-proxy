# F-304: 静的配信のファイルの open・読み取りを、カーネルのキャッシュにあればワーカーの上で行う

**状態: 実装中（feat/beat-nginx）**

## 問題

静的配信のファイルの open・`fstat`・読み取りは、ブロックし得るので専用スレッド（`offload`）で行っていた。
キャッシュの無い経路（`open_file_cache` 無し、本体キャッシュに入らない大きさ・ルート）では要求ごとに offload の往復
（スレッドの起床・通知の fd・完了の受け取り）が 1〜2 回入る。[F-303](F-303-concentrate-connections.md) でワーカーを
少数へ集めると、この往復が 1 要求の費用に占める割合が大きくなった（lab5 の `h2_0_ktls_0_lb_kernel_ofc_0`: 集約で
23.0〜26.9k → 64.8〜65.9k だが、まだ nginx（57.7〜63.3k、`open_file_cache` 無し）の 1 要求の CPU に届いていなかった）。
HTTP/3 では offload を待つ要求の応答が 1 イテレーション遅れる。

また、kTLS 無しの HTTPS の HTTP/1.1 の静的配信（`handle_sendfile_userspace`）は、ヘッダを先に別に書いてから本文を
書いていたので、1 応答に書き込みと TLS のレコードが 1 つずつ多かった。

## 対応

Linux では、ブロックしないと分かっているときだけワーカーの上で行い、ディスクを待つときは従来どおり offload する。

1. **open**（`cache::resolve::open_cached_in_root`・`open_cached_for_request`）: `openat2(RESOLVE_BENEATH |
   RESOLVE_NO_MAGICLINKS | RESOLVE_CACHED)` を `O_NONBLOCK` 付きで呼ぶ。`RESOLVE_CACHED`（5.12 以降）は経路の解決が
   dentry キャッシュで済まないと `EAGAIN` を返す。属性は `statx(AT_STATX_DONT_SYNC)`。通常ファイルは `fcntl(F_SETFL, 0)`
   で `O_NONBLOCK` を外す（FIFO 等を開いても止まらない）。`EAGAIN`・`EXDEV`・`ELOOP`・非対応は `None` を返して従来の経路へ。
   `EINVAL`（`RESOLVE_CACHED` の無いカーネル）を一度見たら以後は試さない。
2. **読み取り**（`runtime::io::File::read_at`・`read_exact_at`、`read_cached_exact`）: `preadv2(RWF_NOWAIT)` でページ
   キャッシュにある分だけ読む。足りない分は 1 回の offload で読む（書き込み先は offload が持つ `Vec`。要求が取り消されても
   呼び出し側のバッファへ書かない）。`RWF_NOWAIT` を受け付けないファイルシステム（overlayfs 等の `EOPNOTSUPP`）は
   一度見たら従来の `pread` に戻す。
3. **静的配信**（`get_static_file_with_content`）: 本体キャッシュへ読む経路は、まず `open_and_read_cached` をワーカーの
   上で試し、だめなら従来の offload（`open_and_read_fast`）。`open_file_cache` の判定（`fetch_file_info`）も同じ open を先に
   試す。開いた fd は従来どおり `open_file_cache` のエントリが持つ（同じ版なら前の fd を使い回す）。
4. **ヘッダと本文**（`handle_sendfile_userspace`）: ヘッダを本文の最初の塊と一緒に `write_all_vectored` で送る。
5. **seccomp**: `preadv2` を許可する（x86_64 327、aarch64 286。`statx`・`fcntl` は既存）。Docker の seccomp プロファイルも。

## 計測

aofish のコンテナ間（[F-303](F-303-concentrate-connections.md) と同じ条件）。`open_file_cache` 無しの静的配信
（`h2_0_ktls_0_lb_kernel_ofc_0`、HTTP/1.1 + TLS、54,576B）。

| ビルド | req/s |
|---|---:|
| v0.12.0（lab7） | 24.0k |
| F-303 の最初の版、F-304 無し（lab6） | 47.9k・66.1k |
| F-303 の改良 + F-304（lab6。制御の変更も含む） | 76.1k・74.4k |
| F-303 + F-304（lab10、3 回） | 68.2k・63.2k・68.7k |
| nginx（同じファイル、`open_file_cache` 無し。lab10） | 52.0k・52.5k・51.1k |

1 要求あたりのサーバの CPU は 77〜86 µs（lab10）。HTTP/3 の静的配信（キャッシュ無し）は、応答が 1 周回遅れる
offload の待ちが無くなった。

## 範囲外・制限

- Linux のみ（BSD・macOS・Windows は従来の offload）。
- overlayfs の上のファイルは `RWF_NOWAIT` を使えないので、読み取りは従来の `pread`（ブロックし得る。ページキャッシュに
  あれば止まらない）。性能計測のスイートは ext4 を bind mount する。
